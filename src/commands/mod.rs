//! Per-command pipelines (design-specs §7).
//!
//! Phase 5 implements find-refs (location-only and --context). Each target
//! runs the degradation ladder (design-specs §9):
//!
//! - engine resolves 1 → `resolved`; >1 (within max) → `multi_resolved`;
//!   >max → `ambiguous`; 0-but-text → `fallback`; 0 → `not_found`.
//! - find-refs emits a dense location list by default, or enclosing-scope
//!   context with `--context`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::cli::{Cli, Command};
use crate::engine::{Engine, SyntacticEngine};
#[cfg(feature = "semantic")]
use crate::engine::SemanticEngine;
use crate::model::{Kind, Resolution};
use crate::output::{self, Record, RefContext, RefLocation, TextWindow, Writer};
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
    let (command_name, targets, query, scope, context) = match &cli.command {
        Command::FindDef { name, scope } => ("find-def", name, Query::Definition, *scope, false),
        Command::FindDecl { name } => ("find-decl", name, Query::Declaration, false, false),
        Command::FindRefs { name, context } => ("find-refs", name, Query::References, false, *context),
    };

    // Collect targets from CLI args + manifest file (design-specs §8.9).
    let mut all_targets: Vec<String> = targets.clone();
    if let Some(manifest_path) = &cli.manifest {
        let content = std::fs::read_to_string(manifest_path)
            .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                all_targets.push(trimmed.to_string());
            }
        }
    }
    // Deduplicate targets while preserving order.
    let mut seen_targets = std::collections::HashSet::new();
    all_targets.retain(|t| seen_targets.insert(t.clone()));

    let mut finder_cfg = build_finder_config(cli);
    // find-decl is header-biased (design-specs §7.2 step 1).
    finder_cfg.prefer_headers = query == Query::Declaration;

    // Select engine: --semantic opts into libclang Stage 2 when the feature is
    // compiled in and a compile_commands.json is available (design-specs §4).
    let syntactic = SyntacticEngine::new();
    #[cfg(feature = "semantic")]
    let semantic;
    #[cfg(feature = "semantic")]
    let engine: &dyn Engine = if cli.semantic {
        let db_path = cli
            .compile_db
            .clone()
            .unwrap_or_else(|| {
                cli.root.first().cloned().unwrap_or_else(|| PathBuf::from("."))
            });
        semantic = SemanticEngine::new(db_path);
        &semantic
    } else {
        &syntactic
    };
    #[cfg(not(feature = "semantic"))]
    let engine: &dyn Engine = {
        if cli.semantic && !cli.quiet {
            eprintln!(
                "cpp-navigator: --semantic requires a build with `--features semantic`; using tree-sitter."
            );
        }
        &syntactic
    };

    let mut records = Vec::new();
    for target in &all_targets {
        let result = search::find_candidates(target, &finder_cfg)?;
        let record = resolve_one(command_name, target, query, scope, context, &result, engine, cli);
        records.push(record);
    }

    // Apply --budget trimming if requested (design-specs §8.10).
    if let Some(budget) = cli.budget {
        records = output::apply_budget(records, budget);
    }

    let stdout = std::io::stdout();
    let mut writer = Writer::new(stdout.lock(), cli.format, cli.legend);
    for record in &records {
        writer.write(record)?;
    }
    writer.finish()?;
    Ok(())
}

/// Apply the degradation ladder to one target's candidate set.
#[allow(clippy::too_many_arguments)]
fn resolve_one(
    command: &str,
    target: &str,
    query: Query,
    scope: bool,
    context: bool,
    result: &FinderResult,
    engine: &dyn Engine,
    cli: &Cli,
) -> Record {
    if result.candidates.is_empty() {
        return Record::not_found(command, target);
    }

    // find-refs: emit dense location list or contextual bodies.
    if query == Query::References {
        return resolve_refs(command, target, context, result, engine, cli);
    }

    let resolutions = match query {
        Query::Definition => engine.definitions(target, &result.candidates),
        Query::Declaration => engine.declarations(target, &result.candidates),
        Query::References => unreachable!(),
    };

    // Semantic engine may return empty when compile_commands.json is absent;
    // fall back to the syntactic engine in that case (design-specs §4).
    let resolutions = if resolutions.is_empty() && engine.name() != "tree-sitter" {
        let syntactic = SyntacticEngine::new();
        match query {
            Query::Definition => syntactic.definitions(target, &result.candidates),
            Query::Declaration => syntactic.declarations(target, &result.candidates),
            Query::References => unreachable!(),
        }
    } else {
        resolutions
    };

    match resolutions.len() {
        0 => text_fallback(command, target, result, cli),
        1 => {
            let base = &resolutions[0];
            let rtype = resolution_type(query, base.symbol.kind);
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
        n if n <= cli.max_results => {
            // Show all matches with full content (user-preferred behavior for overloads).
            let rtype = resolution_type(query, resolutions[0].symbol.kind);
            let mut rec = Record::multi_resolved(command, target, &rtype, &resolutions, n);
            rec.truncated = result.truncated;
            rec
        }
        n => {
            // Too many matches — fall back to ambiguous with locations only.
            let candidates = resolutions.iter().map(to_candidate).collect();
            let mut rec = Record::ambiguous(command, target, candidates);
            rec.message = Some(format!(
                "Found {} candidates (exceeds --max-results {}). Returning locations only.",
                n, cli.max_results
            ));
            rec.truncated = result.truncated;
            rec
        }
    }
}

/// Resolve find-refs: location-only or with enclosing-scope context.
fn resolve_refs(
    command: &str,
    target: &str,
    context: bool,
    result: &FinderResult,
    engine: &dyn Engine,
    cli: &Cli,
) -> Record {
    if context {
        // --context: for each hit, find the enclosing function/template body.
        let mut seen = std::collections::HashSet::new();
        let mut contexts = Vec::new();
        for hit in &result.candidates {
            if let Some(span) = engine.enclosing_scope(&hit.file_path, hit.byte_offset) {
                // Deduplicate by (file, scope start byte) — multiple refs in the
                // same function body should produce only one context entry.
                let key = (hit.file_path.clone(), span.start_byte);
                if !seen.insert(key) {
                    continue;
                }
                let content = match std::fs::read(&hit.file_path) {
                    Ok(src) if span.end_byte <= src.len() => {
                        String::from_utf8_lossy(&src[span.start_byte..span.end_byte]).into_owned()
                    }
                    _ => continue,
                };
                contexts.push(RefContext {
                    file: hit.file_path.to_string_lossy().into_owned(),
                    line: hit.line,
                    scope_start_line: span.start_line,
                    scope_end_line: span.end_line,
                    content,
                });
                if contexts.len() >= cli.max_candidates {
                    break;
                }
            }
        }
        if contexts.is_empty() {
            // No enclosing scope found for any hit — degrade to location-only.
            return resolve_refs_locations(command, target, result);
        }
        let truncated = result.truncated || contexts.len() >= cli.max_candidates;
        Record::references_with_context(command, target, contexts, truncated)
    } else {
        resolve_refs_locations(command, target, result)
    }
}

/// Emit a dense location list for find-refs (default, no --context).
fn resolve_refs_locations(command: &str, target: &str, result: &FinderResult) -> Record {
    // Deduplicate by (file, line) — a single line can have multiple textual
    // matches but we report it once.
    let mut seen = std::collections::HashSet::new();
    let locations: Vec<RefLocation> = result
        .candidates
        .iter()
        .filter(|c| seen.insert((c.file_path.clone(), c.line)))
        .map(|c| RefLocation {
            file: c.file_path.to_string_lossy().into_owned(),
            line: c.line,
        })
        .collect();
    Record::references(command, target, locations, result.truncated)
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
fn expand_to_class_scope(engine: &dyn Engine, r: &Resolution) -> Option<Resolution> {
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

    fn candidates_at(p: &std::path::Path, offsets: &[(usize, usize)]) -> Vec<Candidate> {
        offsets
            .iter()
            .map(|&(line, byte_offset)| Candidate {
                file_path: p.to_path_buf(),
                line,
                byte_offset,
                snippet: String::new(),
            })
            .collect()
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
        let member = &defs[0];
        assert!(String::from_utf8_lossy(&member.content_bytes).starts_with("int area()"));

        let expanded = expand_to_class_scope(&eng, member).unwrap();
        let s = String::from_utf8_lossy(&expanded.content_bytes);
        assert!(s.starts_with("class Widget {"));
        assert!(s.trim_end().ends_with('}'));
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
        assert!(expand_to_class_scope(&eng, &defs[0]).is_none());
    }

    // --- Phase 5: find-refs tests ---

    #[test]
    fn refs_location_only_returns_dense_list() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        let body = "void foo() {}\nvoid bar() { foo(); }\nvoid baz() { foo(); }\n";
        fs::write(&p, body).unwrap();

        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("foo", &finder_cfg).unwrap();
        // foo appears on lines 1 (def), 2 (call), 3 (call)
        assert!(result.candidates.len() >= 3);

        let record = resolve_refs_locations("find-refs", "foo", &result);
        assert_eq!(record.status, output::Status::Resolved);
        assert_eq!(record.resolution_type, "references");
        assert!(record.locations.len() >= 3);
        assert!(!record.truncated);
    }

    #[test]
    fn refs_deduplicates_same_line() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        // Two mentions of `x` on the same line
        let body = "int x = 1;\nvoid f() { x = x + 1; }\n";
        fs::write(&p, body).unwrap();

        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("x", &finder_cfg).unwrap();
        let record = resolve_refs_locations("find-refs", "x", &result);
        // Line 1 has one mention, line 2 has two mentions (deduplicated to one).
        assert_eq!(record.locations.len(), 2);
    }

    #[test]
    fn refs_context_returns_enclosing_function() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        let body = concat!(
            "int helper() { return 42; }\n",
            "void caller1() {\n",
            "    int v = helper();\n",
            "}\n",
            "void caller2() {\n",
            "    helper();\n",
            "}\n",
        );
        fs::write(&p, body).unwrap();

        let eng = SyntacticEngine::new();
        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("helper", &finder_cfg).unwrap();

        // Use resolve_refs with context=true
        let cli = Cli {
            command: Command::FindRefs { name: vec!["helper".into()], context: true },
            root: vec![dir.path().to_path_buf()],
            semantic: false,
            compile_db: None,
            lang: vec![],
            max_candidates: 200,
            max_results: 3,
            window: 10,
            jobs: None,
            no_ignore: false,
            format: crate::cli::Format::Jsonl,
            legend: false,
            manifest: None,
            budget: None,
            quiet: false,
        };
        let record = resolve_refs("find-refs", "helper", true, &result, &eng, &cli);
        assert_eq!(record.resolution_type, "references_with_context");
        // Should have contexts for: helper() def, caller1, caller2
        // but deduped by enclosing scope, so at most 3 distinct scopes.
        assert!(record.contexts.len() >= 2);
        // Each context should contain the function body.
        for ctx in &record.contexts {
            assert!(!ctx.content.is_empty());
            assert!(ctx.scope_start_line > 0);
        }
    }

    #[test]
    fn refs_context_deduplicates_same_scope() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        // Multiple references to `val` in the same function
        let body = "void work() {\n    int val = 1;\n    val++;\n    val *= 2;\n}\n";
        fs::write(&p, body).unwrap();

        let eng = SyntacticEngine::new();
        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("val", &finder_cfg).unwrap();
        assert!(result.candidates.len() >= 3); // val appears 3 times

        let cli = Cli {
            command: Command::FindRefs { name: vec!["val".into()], context: true },
            root: vec![dir.path().to_path_buf()],
            semantic: false,
            compile_db: None,
            lang: vec![],
            max_candidates: 200,
            max_results: 3,
            window: 10,
            jobs: None,
            no_ignore: false,
            format: crate::cli::Format::Jsonl,
            legend: false,
            manifest: None,
            budget: None,
            quiet: false,
        };
        let record = resolve_refs("find-refs", "val", true, &result, &eng, &cli);
        // All refs in the same function → deduplicated to one context entry.
        assert_eq!(record.contexts.len(), 1);
        assert!(record.contexts[0].content.contains("int val = 1;"));
    }

    #[test]
    fn refs_across_multiple_files() {
        let dir = TempDir::new().unwrap();
        let p1 = dir.path().join("a.cpp");
        let p2 = dir.path().join("b.cpp");
        fs::write(&p1, "void target() {}\n").unwrap();
        fs::write(&p2, "extern void target();\nvoid use() { target(); }\n").unwrap();

        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("target", &finder_cfg).unwrap();
        let record = resolve_refs_locations("find-refs", "target", &result);
        // At least 3 locations: def in a.cpp, decl in b.cpp, call in b.cpp
        assert!(record.locations.len() >= 3);
        // Files should be ordered deterministically.
        let files: Vec<&str> = record.locations.iter().map(|l| l.file.as_str()).collect();
        let mut sorted = files.clone();
        sorted.sort();
        assert_eq!(files, sorted);
    }

    // --- Phase 5: multi-resolved (overload show-all) tests ---

    #[test]
    fn overloads_within_max_results_show_full_content() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        let body = "void process(int x) { x++; }\nvoid process(double y) { y *= 2; }\n";
        fs::write(&p, body).unwrap();

        let eng = SyntacticEngine::new();
        let candidates = one_candidate(&p);
        let resolutions = eng.definitions("process", &candidates);
        assert_eq!(resolutions.len(), 2);

        // With max_results=3, both should be shown in full.
        let record = Record::multi_resolved(
            "find-def", "process", "function_definition", &resolutions, 2,
        );
        assert_eq!(record.status, output::Status::Resolved);
        assert_eq!(record.results.len(), 2);
        assert!(record.results[0].content.contains("x++"));
        assert!(record.results[1].content.contains("y *= 2"));
    }

    #[test]
    fn overloads_exceeding_max_results_show_ambiguous() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        let body = concat!(
            "void f(int a) {}\n",
            "void f(double a) {}\n",
            "void f(char a) {}\n",
            "void f(long a) {}\n",
        );
        fs::write(&p, body).unwrap();

        let eng = SyntacticEngine::new();
        let candidates = one_candidate(&p);
        let resolutions = eng.definitions("f", &candidates);
        assert_eq!(resolutions.len(), 4);

        // Simulate what resolve_one does with max_results=3
        let max_results = 3;
        let n = resolutions.len();
        assert!(n > max_results);
        let candidates_out: Vec<output::Candidate> = resolutions.iter().map(to_candidate).collect();
        let record = Record::ambiguous("find-def", "f", candidates_out);
        assert_eq!(record.status, output::Status::Ambiguous);
        assert_eq!(record.candidates.len(), 4);
    }

    // --- Regression tests inspired by mepsplatform patterns ---

    #[test]
    fn refs_finds_method_calls_through_pointer() {
        // Pattern from mepsplatform: m_pEdit->GetWindowText(str)
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("dlg.cpp");
        let body = concat!(
            "class CDialog {\n",
            "public:\n",
            "    void GetWindowText(CString& s);\n",
            "};\n",
            "void OnOK() {\n",
            "    CString str;\n",
            "    m_pEdit->GetWindowText(str);\n",
            "}\n",
            "void OnChange() {\n",
            "    CString s;\n",
            "    m_pEdit->GetWindowText(s);\n",
            "    m_label.GetWindowText(s);\n",
            "}\n",
        );
        fs::write(&p, body).unwrap();

        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("GetWindowText", &finder_cfg).unwrap();
        let record = resolve_refs_locations("find-refs", "GetWindowText", &result);
        // Should find all 4 mentions: declaration + 3 calls
        assert_eq!(record.locations.len(), 4);
    }

    #[test]
    fn refs_context_finds_method_in_class_member_function() {
        // Pattern from mepsplatform: OnCreate defined as CMainFrame::OnCreate
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("frame.cpp");
        let body = concat!(
            "int CMainFrame::OnCreate(LPCREATESTRUCT lp) {\n",
            "    if (CFrameWnd::OnCreate(lp) == -1)\n",
            "        return -1;\n",
            "    return 0;\n",
            "}\n",
            "int CChildView::OnCreate(LPCREATESTRUCT lp) {\n",
            "    OnCreate(lp);\n",
            "    return 0;\n",
            "}\n",
        );
        fs::write(&p, body).unwrap();

        let eng = SyntacticEngine::new();
        let finder_cfg = FinderConfig {
            roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let result = search::find_candidates("OnCreate", &finder_cfg).unwrap();
        assert!(result.candidates.len() >= 4); // multiple mentions

        let cli = Cli {
            command: Command::FindRefs { name: vec!["OnCreate".into()], context: true },
            root: vec![dir.path().to_path_buf()],
            semantic: false,
            compile_db: None,
            lang: vec![],
            max_candidates: 200,
            max_results: 3,
            window: 10,
            jobs: None,
            no_ignore: false,
            format: crate::cli::Format::Jsonl,
            legend: false,
            manifest: None,
            budget: None,
            quiet: false,
        };
        let record = resolve_refs("find-refs", "OnCreate", true, &result, &eng, &cli);
        assert_eq!(record.resolution_type, "references_with_context");
        // Should deduplicate to 2 distinct function scopes.
        assert_eq!(record.contexts.len(), 2);
    }

    #[test]
    fn find_def_qualified_shows_specific_overload() {
        // Pattern: CMainFrame::OnCreate vs CChildView::OnCreate
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("frame.cpp");
        let body = concat!(
            "int CMainFrame::OnCreate(LPCREATESTRUCT lp) {\n",
            "    return 0;\n",
            "}\n",
            "int CChildView::OnCreate(LPCREATESTRUCT lp) {\n",
            "    return 0;\n",
            "}\n",
        );
        fs::write(&p, body).unwrap();

        let eng = SyntacticEngine::new();
        let candidates = one_candidate(&p);
        // Qualified name narrows to one.
        let res = eng.definitions("CMainFrame::OnCreate", &candidates);
        assert_eq!(res.len(), 1);
        assert!(String::from_utf8_lossy(&res[0].content_bytes).contains("CMainFrame::OnCreate"));
    }

    #[test]
    fn refs_not_found_for_missing_symbol() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("a.cpp");
        fs::write(&p, "void other() {}\n").unwrap();

        let result = FinderResult {
            candidates: vec![],
            truncated: false,
        };
        let record = resolve_refs_locations("find-refs", "missing", &result);
        // Empty locations is valid — caller should check not_found first.
        assert!(record.locations.is_empty());
    }

    // --- Phase 6: manifest and budget tests ---

    #[test]
    fn manifest_parses_targets_and_deduplicates() {
        let dir = TempDir::new().unwrap();
        let manifest = dir.path().join("queries.txt");
        fs::write(
            &manifest,
            "alpha\nbeta\n# comment line\nalpha\ngamma\n",
        )
        .unwrap();

        // Simulate what dispatch does: read manifest and deduplicate.
        let content = fs::read_to_string(&manifest).unwrap();
        let mut targets: Vec<String> = vec!["alpha".to_string()]; // from CLI
        for line in content.lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                targets.push(trimmed.to_string());
            }
        }
        let mut seen = std::collections::HashSet::new();
        targets.retain(|t| seen.insert(t.clone()));

        assert_eq!(targets, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn budget_trims_records_correctly() {
        // Create records that exceed the budget, verify trimming.
        let rec1 = Record::not_found("find-def", "big_target_name_1");
        let mut rec2 = Record::not_found("find-def", "big_target_name_2");
        rec2.content = Some("x".repeat(1000)); // make it large
        let records = vec![rec1, rec2];
        let trimmed = output::apply_budget(records, 50);
        assert!(trimmed.len() <= 2);
        // At least one record survives.
        assert!(!trimmed.is_empty());
        assert!(trimmed.last().unwrap().budget_trimmed);
    }
}
