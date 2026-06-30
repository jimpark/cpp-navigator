//! Interactive tree browser (`--interactive`/`-i`): `n`/`p` jump to the
//! first line of the next/previous file (clamping at the ends with a
//! status message, never wrapping). An alternative to the
//! JSONL/bundle/human writers that puts the same resolved hits up as a
//! collapsible, navigable tree in the terminal instead of printing them.
//! Enter opens the line under the cursor — handed to a detected IDE's
//! integrated terminal (VS Code, a JetBrains IDE, Zed) when running inside
//! one, or spawned as a configured terminal editor otherwise.

mod browser;
mod editor;
mod hits;
mod ide;
mod tree;

use std::io::IsTerminal;

use anyhow::{bail, Result};

use crate::output::Record;

/// Build and run the browser over a finished batch of records. Call this
/// instead of writing to stdout when `--interactive` is set.
pub fn run(
    records: &[Record],
    command: &str,
    targets: &[String],
    editor_override: Option<&str>,
    template_override: Option<&str>,
) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("--interactive needs an interactive terminal");
    }

    let collected = hits::collect_hits(records);
    if collected.is_empty() {
        println!("cpp-navigator: no results for {} {}", command, targets.join(" "));
        return Ok(());
    }
    let mut cache = hits::FileCache::new();
    let lines = hits::expand_to_lines(&collected, &mut cache);

    let ide = ide::detect();
    let (editor_argv, line_template) = editor::resolve(editor_override, template_override);

    let title = format!("cpp-navigator {command} {}", targets.join(" "));
    let mut app = browser::Browser::new(title, lines, ide, editor_argv, line_template);
    app.run().map_err(anyhow::Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Engine, SyntacticEngine};
    use crate::output::Record;
    use crate::search::{self, FinderConfig};
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample")
    }

    fn def_record(target: &str) -> Record {
        let finder_cfg = FinderConfig { roots: vec![fixture_root()], ..Default::default() };
        let result = search::find_candidates(target, &finder_cfg).unwrap();
        let eng = SyntacticEngine::new();
        let defs = eng.definitions(target, &result.candidates);
        assert!(!defs.is_empty(), "expected at least one definition for {target}");
        if defs.len() == 1 {
            Record::resolved("find-def", target, "class_definition", &defs[0])
        } else {
            let n = defs.len();
            Record::multi_resolved("find-def", target, "function_definition", &defs, n)
        }
    }

    fn refs_record(target: &str) -> Record {
        let finder_cfg = FinderConfig { roots: vec![fixture_root()], ..Default::default() };
        let result = search::find_candidates(target, &finder_cfg).unwrap();
        let locations = result
            .candidates
            .iter()
            .map(|c| crate::output::RefLocation {
                file: c.file_path.to_string_lossy().into_owned(),
                line: c.line,
            })
            .collect();
        Record::references("find-refs", target, locations, false)
    }

    /// Builds the same hits -> lines -> tree -> rows pipeline `run()` uses,
    /// against real engine output, and checks the tree's invariants: every
    /// row id is unique (no collapse()/jump target collisions) and jump
    /// numbers are sequential — without needing a real terminal.
    #[test]
    fn pipeline_produces_unique_ids_and_sequential_jump_numbers() {
        for rec in [def_record("Widget"), refs_record("Widget")] {
            let collected = hits::collect_hits(&[rec]);
            assert!(!collected.is_empty());
            let mut cache = hits::FileCache::new();
            let lines = hits::expand_to_lines(&collected, &mut cache);
            assert!(lines.len() >= collected.len());

            let node = tree::build_tree(&lines);
            let mut expanded = HashSet::new();
            tree::all_folder_paths(&node, &mut expanded);
            let rows = tree::build_visible(&node, &expanded);

            let mut ids = HashSet::new();
            for r in &rows {
                assert!(ids.insert(r.id.clone()), "duplicate row id {}", r.id);
            }
            let numbers: Vec<usize> = rows.iter().filter_map(|r| r.number).collect();
            let expected: Vec<usize> = (1..=numbers.len()).collect();
            assert_eq!(numbers, expected, "jump numbers should be 1..=N in display order");

            // Every line row's "collapse to parent folder" target must exist
            // among the folder rows whenever the file is nested in a dir.
            for row in rows.iter().filter(|r| !r.is_folder) {
                let file_path = row.id.trim_start_matches("L:").split('\0').next().unwrap();
                if let Some((parent, _)) = file_path.rsplit_once('/') {
                    let target = format!("F:{parent}");
                    assert!(
                        rows.iter().any(|r| r.id == target),
                        "missing parent folder row {target} for line {file_path}"
                    );
                }
            }
        }
    }

    #[test]
    fn not_found_record_yields_no_hits() {
        let rec = Record::not_found("find-def", "NoSuchSymbol");
        assert!(hits::collect_hits(&[rec]).is_empty());
    }
}
