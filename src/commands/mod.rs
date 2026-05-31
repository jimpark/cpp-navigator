//! Per-command pipelines (design-specs §7).
//!
//! Phase 2 wires the syntactic engine (Stage 1) into `find-def`/`find-decl`.
//! Each target runs the degradation ladder (design-specs §9):
//!   engine resolves 1 → `resolved`; >1 → `ambiguous`; 0 (but text hits) →
//!   `fallback` text window; 0 text hits → `not_found`.
//! `find-refs` keeps the Phase 1 location/window behavior until Phase 5.

use std::path::PathBuf;

use anyhow::Result;

use crate::cli::{Cli, Command};
use crate::engine::{Engine, SyntacticEngine};
use crate::model::{Kind, Resolution};
use crate::output::{self, Record, TextWindow, Writer};
use crate::search::{self, FinderConfig, FinderResult, DEFAULT_EXTENSIONS};

/// Which resolution a command asks the engine for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Query {
    Definition,
    Declaration,
    /// Reference search — handled by the text path (Phase 5 adds context).
    References,
}

/// Run the selected command, writing records to stdout.
pub fn dispatch(cli: &Cli) -> Result<()> {
    let (command_name, targets, query, scope) = match &cli.command {
        Command::FindDef { name, scope } => ("find-def", name, Query::Definition, *scope),
        Command::FindDecl { name } => ("find-decl", name, Query::Declaration, false),
        Command::FindRefs { name, .. } => ("find-refs", name, Query::References, false),
    };

    let mut finder_cfg = build_finder_config(cli);
    // find-decl is header-biased (design-specs §7.2 step 1).
    finder_cfg.prefer_headers = query == Query::Declaration;
    let engine = SyntacticEngine::new();

    let stdout = std::io::stdout();
    let mut writer = Writer::new(stdout.lock(), cli.format, cli.legend);

    for target in targets {
        let result = search::find_candidates(target, &finder_cfg)?;
        let record = resolve_one(command_name, target, query, scope, &result, &engine, cli);
        writer.write(&record)?;
    }

    writer.finish()?;
    Ok(())
}

/// Apply the degradation ladder to one target's candidate set.
fn resolve_one(
    command: &str,
    target: &str,
    query: Query,
    scope: bool,
    result: &FinderResult,
    engine: &SyntacticEngine,
    cli: &Cli,
) -> Record {
    if result.candidates.is_empty() {
        return Record::not_found(command, target);
    }

    // find-refs does not parse for boundaries in v1 (Phase 5); emit a window.
    if query == Query::References {
        return text_fallback(command, target, result, cli);
    }

    let resolutions = match query {
        Query::Definition => engine.definitions(target, &result.candidates),
        Query::Declaration => engine.declarations(target, &result.candidates),
        Query::References => unreachable!(),
    };

    match resolutions.len() {
        0 => text_fallback(command, target, result, cli),
        1 => {
            let base = &resolutions[0];
            let rtype = resolution_type(query, base.symbol.kind);
            // `--scope` (find-def): widen a resolved member to its enclosing
            // class/struct. Graceful no-op when the match is not inside one
            // (free function, out-of-line member) — design §7.1 step 3.
            let expanded = if scope && query == Query::Definition {
                expand_to_class_scope(engine, base)
            } else {
                None
            };
            let mut rec = match &expanded {
                Some(e) => {
                    let mut rec = Record::resolved(command, target, &rtype, e);
                    rec.message =
                        Some("Expanded to the enclosing class/struct scope (--scope).".to_string());
                    rec
                }
                None => Record::resolved(command, target, &rtype, base),
            };
            rec.truncated = result.truncated;
            rec
        }
        _ => {
            let candidates = resolutions.iter().map(to_candidate).collect();
            let mut rec = Record::ambiguous(command, target, candidates);
            rec.truncated = result.truncated;
            rec
        }
    }
}

/// Build the `resolution_type` string for a resolved record.
fn resolution_type(query: Query, kind: Kind) -> String {
    match query {
        Query::Declaration => "declaration".to_string(),
        Query::Definition => match kind {
            Kind::Function | Kind::Method => "function_definition".to_string(),
            Kind::Variable | Kind::Member => "variable_definition".to_string(),
            Kind::Template => "template_definition".to_string(),
            Kind::Class => "class_definition".to_string(),
            Kind::Struct => "struct_definition".to_string(),
            Kind::Macro => "macro_definition".to_string(),
        },
        Query::References => "reference".to_string(),
    }
}

/// Widen a resolved member to its enclosing `class`/`struct` (or wrapping
/// `template`) span, re-slicing `content` from disk so the byte-fidelity
/// contract (§8.4) holds for the wider span. Returns `None` when the match is
/// not lexically inside a class/struct, leaving the member result unchanged.
fn expand_to_class_scope(engine: &SyntacticEngine, r: &Resolution) -> Option<Resolution> {
    let span = engine.enclosing_class_scope(&r.source_ref.file_path, r.source_ref.span.start_byte)?;
    let src = std::fs::read(&r.source_ref.file_path).ok()?;
    if span.end_byte > src.len() || span.start_byte > span.end_byte {
        return None;
    }
    let mut out = r.clone();
    out.content_bytes = src[span.start_byte..span.end_byte].to_vec();
    out.source_ref.span = span;
    Some(out)
}

/// Map a resolution to an ambiguous-candidate location (design-specs §8.5).
fn to_candidate(r: &Resolution) -> output::Candidate {
    let snippet = String::from_utf8_lossy(&r.content_bytes)
        .lines()
        .next()
        .unwrap_or("")
        .to_string();
    output::Candidate {
        file_path: r.source_ref.file_path.to_string_lossy().into_owned(),
        line: r.source_ref.span.start_line,
        snippet: Some(snippet),
    }
}

/// Degrade to a verbatim ±window text buffer around the first textual hit
/// (design-specs §8.6).
fn text_fallback(command: &str, target: &str, result: &FinderResult, cli: &Cli) -> Record {
    let hit = &result.candidates[0];
    let (buffer, before, after) =
        read_window(&hit.file_path, hit.line, cli.window).unwrap_or_else(|_| (hit.snippet.clone(), 0, 0));
    let window = TextWindow {
        file_path: hit.file_path.to_string_lossy().into_owned(),
        approximate_line: hit.line,
        before,
        after,
        content_buffer: buffer,
    };
    let msg = match command {
        "find-refs" => "Reference locations as a raw text window.",
        _ => "Semantic extraction unavailable for this target; returning raw text window.",
    };
    let mut rec = Record::fallback(command, target, window, msg);
    rec.truncated = result.truncated;
    rec
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
        prefer_headers: false,
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

    #[test]
    fn def_resolution_type_by_kind() {
        assert_eq!(resolution_type(Query::Definition, Kind::Function), "function_definition");
        assert_eq!(resolution_type(Query::Definition, Kind::Template), "template_definition");
        assert_eq!(resolution_type(Query::Declaration, Kind::Function), "declaration");
    }

    use crate::engine::Engine;
    use crate::search::Candidate;

    fn one_candidate(p: &std::path::Path) -> Vec<Candidate> {
        vec![Candidate {
            file_path: p.to_path_buf(),
            line: 1,
            byte_offset: 0,
            snippet: String::new(),
        }]
    }

    #[test]
    fn scope_expands_inline_member_with_byte_fidelity() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("w.hpp");
        let body = "class Widget {\npublic:\n    int area() { return w * h; }\n    int w, h;\n};\n";
        fs::write(&p, body).unwrap();
        let eng = SyntacticEngine::new();

        let defs = eng.definitions("area", &one_candidate(&p));
        assert_eq!(defs.len(), 1);
        // Without scope, the member span is just the method.
        let member = &defs[0];
        assert!(String::from_utf8_lossy(&member.content_bytes).starts_with("int area()"));

        // With scope, expand to the whole class.
        let expanded = expand_to_class_scope(&eng, member).unwrap();
        let s = String::from_utf8_lossy(&expanded.content_bytes);
        assert!(s.starts_with("class Widget {"));
        assert!(s.trim_end().ends_with('}'));
        // Byte-fidelity: re-slice from disk equals reported content (§8.4).
        let disk = fs::read(&p).unwrap();
        let span = &expanded.source_ref.span;
        assert_eq!(&disk[span.start_byte..span.end_byte], expanded.content_bytes.as_slice());
    }

    #[test]
    fn scope_is_noop_for_free_function() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        fs::write(&p, "int add(int a, int b) {\n    return a + b;\n}\n").unwrap();
        let eng = SyntacticEngine::new();
        let defs = eng.definitions("add", &one_candidate(&p));
        assert_eq!(defs.len(), 1);
        // No enclosing class → no expansion.
        assert!(expand_to_class_scope(&eng, &defs[0]).is_none());
    }
}
