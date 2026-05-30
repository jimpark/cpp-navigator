//! Per-command pipelines (design-specs §7).
//!
//! Phase 1 wires Stage 0 (the candidate finder) into the I/O path. With no
//! semantic engine yet (Phase 2), a textual hit degrades to the `fallback`
//! rung of the ladder (design-specs §9): we emit a verbatim line-window around
//! the first hit. `not_found` is still produced when there are zero hits.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{Cli, Command};
use crate::output::{Record, TextWindow, Writer};
use crate::search::{self, FinderConfig, DEFAULT_EXTENSIONS};

/// Run the selected command, writing records to stdout.
pub fn dispatch(cli: &Cli) -> Result<()> {
    let (command_name, targets) = match &cli.command {
        Command::FindDef { name, .. } => ("find-def", name),
        Command::FindDecl { name } => ("find-decl", name),
        Command::FindRefs { name, .. } => ("find-refs", name),
    };

    let finder_cfg = build_finder_config(cli);

    let stdout = std::io::stdout();
    let mut writer = Writer::new(stdout.lock(), cli.format, cli.legend);

    for target in targets {
        let result = search::find_candidates(target, &finder_cfg)?;
        let record = if result.candidates.is_empty() {
            Record::not_found(command_name, target)
        } else {
            // TODO(phase 2+): hand candidates to the syntactic engine for exact
            // boundaries. Until then, degrade to a verbatim text window.
            let hit = &result.candidates[0];
            let (buffer, before, after) =
                read_window(&hit.file_path, hit.line, cli.window)
                    .unwrap_or_else(|_| (hit.snippet.clone(), 0, 0));
            let window = TextWindow {
                file_path: hit.file_path.to_string_lossy().into_owned(),
                approximate_line: hit.line,
                before,
                after,
                content_buffer: buffer,
            };
            let mut rec = Record::fallback(
                command_name,
                target,
                window,
                "Semantic extraction not yet available; returning raw text window.",
            );
            rec.truncated = result.truncated;
            rec
        };
        writer.write(&record)?;
    }

    writer.finish()?;
    Ok(())
}

/// Translate CLI globals into a [`FinderConfig`] (design-specs §12).
fn build_finder_config(cli: &Cli) -> FinderConfig {
    let roots = if cli.root.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        cli.root.clone()
    };
    let extensions = if cli.lang.is_empty() {
        DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect()
    } else {
        cli.lang.clone()
    };
    FinderConfig {
        roots,
        extensions,
        respect_ignore: !cli.no_ignore,
        max_candidates: cli.max_candidates,
        threads: cli.jobs,
    }
}

/// Read a verbatim window of `±window` lines around (1-based) `line`.
///
/// Returns the joined text plus the actual number of lines included before and
/// after the target (which can be smaller than `window` near file edges). The
/// text is byte-faithful per line; only line splitting/rejoining occurs, with
/// `\n` separators preserved between retained lines.
fn read_window(path: &PathBuf, line: usize, window: usize) -> Result<(String, usize, usize)> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Ok((String::new(), 0, 0));
    }
    let idx = line.saturating_sub(1).min(lines.len() - 1);
    let start = idx.saturating_sub(window);
    let end = (idx + window + 1).min(lines.len());
    let before = idx - start;
    let after = end - idx - 1;
    let buffer = lines[start..end].join("\n");
    Ok((buffer, before, after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn window_clamps_at_file_start() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        fs::write(&p, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let (buf, before, after) = read_window(&p, 1, 10).unwrap();
        assert_eq!(before, 0);
        assert_eq!(after, 4);
        assert_eq!(buf, "l1\nl2\nl3\nl4\nl5");
    }

    #[test]
    fn window_centers_on_line() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        fs::write(&p, "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let (buf, before, after) = read_window(&p, 3, 1).unwrap();
        assert_eq!(before, 1);
        assert_eq!(after, 1);
        assert_eq!(buf, "l2\nl3\nl4");
    }
}
