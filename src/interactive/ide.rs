//! Detects the editor whose integrated terminal we're running in, and hands
//! it a file:line to open without ever taking the terminal away from the
//! browser. Three editors are recognized, each through the environment
//! variable it injects into its integrated terminal:
//!
//!   VS Code     TERM_PROGRAM == "vscode"
//!   JetBrains   TERMINAL_EMULATOR == "JetBrains-JediTerm" (CLion, IntelliJ, ...)
//!   Zed         TERM_PROGRAM == "zed"  or  ZED_TERM == "true"
//!
//! Every route here is fire-and-forget (a URL hand-off or a CLI launcher
//! that talks to the already-running instance), so the caller never needs
//! to suspend its raw-mode terminal session the way it must for a plain
//! editor like vim.

use std::process::{Command, Stdio};

pub enum Ide {
    VsCode,
    JetBrains { launcher: String, label: String },
    Zed,
}

impl Ide {
    pub fn label(&self) -> &str {
        match self {
            Ide::VsCode => "VS Code",
            Ide::JetBrains { label, .. } => label,
            Ide::Zed => "Zed",
        }
    }

    /// Open `path` at `line`/`column` in the running editor and return at
    /// once. `Err` only when the CLI launcher isn't on PATH.
    pub fn open(&self, path: &str, line: usize, column: usize) -> std::io::Result<()> {
        match self {
            Ide::VsCode => {
                open_url(&format!("vscode://file{path}:{line}:{column}"));
                Ok(())
            }
            Ide::JetBrains { launcher, .. } => {
                // The launchers share a `--line <n> <path>` syntax. They
                // don't reliably accept a column flag, so it's dropped
                // rather than risk a bad flag aborting the open.
                spawn_detached(launcher, &["--line", &line.to_string(), path])
            }
            Ide::Zed => spawn_detached("zed", &[&format!("{path}:{line}:{column}")]),
        }
    }

    pub fn open_error(&self) -> String {
        match self {
            Ide::Zed => "zed not found on PATH (run 'zed: install cli')".to_string(),
            Ide::JetBrains { launcher, .. } => format!("{launcher} not found on PATH"),
            Ide::VsCode => "could not hand off to VS Code".to_string(),
        }
    }
}

fn spawn_detached(program: &str, args: &[&str]) -> std::io::Result<()> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
}

fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = spawn_detached("open", &[url]);
    #[cfg(target_os = "windows")]
    let _ = Command::new("cmd")
        .args(["/C", "start", "", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let _ = spawn_detached("xdg-open", &[url]);
}

/// JetBrains products, in the order guessed when nothing else identifies
/// which one owns the terminal: (substrings to match in an env hint,
/// launcher binary, display label).
const JETBRAINS: &[(&[&str], &str, &str)] = &[
    (&["intellij"], "idea", "IntelliJ IDEA"),
    (&["pycharm"], "pycharm", "PyCharm"),
    (&["clion"], "clion", "CLion"),
    (&["webstorm"], "webstorm", "WebStorm"),
    (&["goland"], "goland", "GoLand"),
    (&["rider"], "rider", "Rider"),
    (&["phpstorm"], "phpstorm", "PhpStorm"),
    (&["rubymine"], "rubymine", "RubyMine"),
    (&["datagrip"], "datagrip", "DataGrip"),
];

fn jetbrains_from_blob(blob: &str) -> Option<(&'static str, &'static str)> {
    let blob = blob.to_lowercase();
    JETBRAINS
        .iter()
        .find(|(substrings, _, _)| substrings.iter().any(|s| blob.contains(s)))
        .map(|(_, launcher, label)| (*launcher, *label))
}

/// Which JetBrains IDE owns this terminal. macOS sets `XPC_SERVICE_NAME` to
/// the bundle id, which identifies the product directly. Elsewhere there's
/// no equivalent signal available without walking the process tree, so this
/// falls back to the first known launcher found on PATH, and finally to a
/// generic guess — same limitation the sibling git-grep/git-open tools have.
fn jetbrains_owner() -> Ide {
    if let Some(found) = std::env::var("XPC_SERVICE_NAME").ok().and_then(|v| jetbrains_from_blob(&v)) {
        return Ide::JetBrains { launcher: found.0.to_string(), label: found.1.to_string() };
    }
    for (_, launcher, label) in JETBRAINS {
        if which(launcher) {
            return Ide::JetBrains { launcher: launcher.to_string(), label: label.to_string() };
        }
    }
    Ide::JetBrains { launcher: "idea".to_string(), label: "JetBrains IDE".to_string() }
}

fn which(program: &str) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else { return false };
    let exe_suffixes: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::split_paths(&path_var).any(|dir| {
        exe_suffixes.iter().any(|suffix| dir.join(format!("{program}{suffix}")).is_file())
    })
}

/// The editor whose integrated terminal we're running in, or `None` for a
/// plain terminal — in which case the caller should spawn the configured
/// editor itself instead.
pub fn detect() -> Option<Ide> {
    let term_program = std::env::var("TERM_PROGRAM").ok();
    if term_program.as_deref() == Some("vscode") {
        return Some(Ide::VsCode);
    }
    if std::env::var("TERMINAL_EMULATOR").ok().as_deref() == Some("JetBrains-JediTerm") {
        return Some(jetbrains_owner());
    }
    if term_program.as_deref() == Some("zed") || std::env::var("ZED_TERM").ok().as_deref() == Some("true") {
        return Some(Ide::Zed);
    }
    None
}
