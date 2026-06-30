//! Resolves which terminal editor to spawn (when we're not inside a
//! detected IDE) and how to tell it to open at a specific line.
//!
//! Editor precedence: `--editor`, then `$VISUAL`, then `$EDITOR`, then a
//! platform default (`notepad` on Windows, `vi` elsewhere). The line
//! template is guessed from the editor's basename (`code -g` wants
//! `{file}:{line}:{column}`, `vim` wants `+{line} {file}`, ...) and can be
//! overridden outright with `--editor-template` for anything unrecognized.

/// The default line template (vim-style) used when nothing more specific
/// matches the editor's basename.
const DEFAULT_TEMPLATE: &str = "+{line} {file}";

/// Resolve the editor command (already split into argv) and its line
/// template. `editor_override`/`template_override` come from `--editor`/
/// `--editor-template`; either may be empty to fall through to the
/// environment / heuristic.
pub fn resolve(editor_override: Option<&str>, template_override: Option<&str>) -> (Vec<String>, String) {
    let editor = editor_override
        .map(str::to_string)
        .or_else(|| std::env::var("VISUAL").ok())
        .or_else(|| std::env::var("EDITOR").ok())
        .unwrap_or_else(|| if cfg!(windows) { "notepad".to_string() } else { "vi".to_string() });

    let argv = split_shell(&editor);
    let template = template_override
        .map(str::to_string)
        .unwrap_or_else(|| template_for(&argv));
    (argv, template)
}

/// Guess a `{file}`/`{line}`/`{column}` template from the editor's basename.
fn template_for(argv: &[String]) -> String {
    let name = argv
        .first()
        .map(|s| s.as_str())
        .unwrap_or("")
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim_end_matches(".exe")
        .to_lowercase();
    match name.as_str() {
        "code" | "code-insiders" | "codium" => "-g {file}:{line}:{column}".to_string(),
        "subl" | "sublime_text" | "zed" => "{file}:{line}:{column}".to_string(),
        "notepad++" | "notepad++.exe" => "-n{line} {file}".to_string(),
        "emacsclient" | "emacs" => "+{line} {file}".to_string(),
        "vim" | "nvim" | "vi" | "nano" => "+{line} {file}".to_string(),
        "notepad" => "{file}".to_string(),
        _ => DEFAULT_TEMPLATE.to_string(),
    }
}

/// Build the full argv to open `path` at `line`/`column`: split the line
/// template shell-style and substitute into each token, so a path with
/// spaces stays one argument.
pub fn open_args(editor_argv: &[String], line_template: &str, path: &str, line: usize, column: usize) -> Vec<String> {
    let mut argv = editor_argv.to_vec();
    let tokens = split_shell(line_template);
    let tokens = if tokens.is_empty() { vec!["{file}".to_string()] } else { tokens };
    for t in tokens {
        argv.push(
            t.replace("{file}", path)
                .replace("{line}", &line.to_string())
                .replace("{column}", &column.to_string()),
        );
    }
    argv
}

/// Minimal shell-style tokenizer: splits on whitespace, honoring single and
/// double quotes so a quoted editor command stays one argument. Not a full
/// POSIX shell grammar — templates and editor commands here are short and
/// user-controlled, so this is deliberately simple rather than exhaustive.
fn split_shell(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut has_token = false;
    for c in s.chars() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has_token = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has_token = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has_token {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_match_known_editors() {
        assert_eq!(template_for(&["code".to_string(), "-g".to_string()]), "-g {file}:{line}:{column}");
        assert_eq!(template_for(&["vim".to_string()]), "+{line} {file}");
        assert_eq!(template_for(&["/usr/bin/nvim".to_string()]), "+{line} {file}");
        assert_eq!(template_for(&["unknown-editor".to_string()]), DEFAULT_TEMPLATE);
    }

    #[test]
    fn open_args_substitutes_and_keeps_spaced_paths_single() {
        let argv = open_args(
            &["code".to_string()],
            "-g {file}:{line}:{column}",
            "C:/a dir/file.cpp",
            42,
            1,
        );
        assert_eq!(argv, vec!["code", "-g", "C:/a dir/file.cpp:42:1"]);
    }

    #[test]
    fn split_shell_honors_quotes() {
        assert_eq!(split_shell("code -g"), vec!["code", "-g"]);
        assert_eq!(split_shell("'my editor' --wait"), vec!["my editor", "--wait"]);
    }
}
