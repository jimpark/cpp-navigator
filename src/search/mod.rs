//! Candidate finder — Stage 0 of the pipeline (design-specs §5, §10).
//!
//! Narrows a potentially huge tree to the handful of files that *textually
//! mention* the target identifier, honoring ignore rules. Built on ripgrep's
//! own library crates (`ignore` for the gitignore-aware parallel walk, `grep`
//! for line searching) so the binary stays self-contained — there is no
//! external `rg` process and no network path (design-specs §3.1).
//!
//! This stage is a *prefilter*: it maximizes recall by matching the bare final
//! identifier component (e.g. `f` for a `A::B::f` target). Precise, qualified
//! name matching is the syntactic engine's job (Phase 2).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use grep::regex::RegexMatcher;
use grep::searcher::{Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, WalkState};

/// Default C/C++ source and header extensions (design-specs §12).
pub const DEFAULT_EXTENSIONS: &[&str] = &["c", "cc", "cpp", "cxx", "h", "hpp", "hh", "hxx"];

/// A textual hit produced by the candidate finder, before any parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub file_path: PathBuf,
    /// 1-based line of the hit.
    pub line: usize,
    /// Byte offset of the start of the matching line within the file.
    pub byte_offset: usize,
    /// The raw matched line (trailing newline stripped), for ambiguous-mode
    /// snippets. Not a fidelity payload — the engine re-reads bytes for that.
    pub snippet: String,
}

/// Inputs to a candidate search, derived from the CLI globals (design-specs §12).
#[derive(Clone, Debug)]
pub struct FinderConfig {
    /// Search roots. Empty means the current directory.
    pub roots: Vec<PathBuf>,
    /// Extensions to include, without the leading dot.
    pub extensions: Vec<String>,
    /// Honor `.gitignore`/`.ignore` and skip hidden/binary files.
    pub respect_ignore: bool,
    /// Cap on the number of distinct candidate *files* (design-specs §10).
    pub max_candidates: usize,
    /// Walker/searcher threads. `None` lets `ignore` pick (≈ #cores).
    pub threads: Option<usize>,
    /// find-decl: run a header-only pass first, falling back to all files when
    /// no header declares the symbol (design-specs §7.2 step 1).
    pub prefer_headers: bool,
}

impl Default for FinderConfig {
    fn default() -> Self {
        FinderConfig {
            roots: vec![PathBuf::from(".")],
            extensions: DEFAULT_EXTENSIONS.iter().map(|s| s.to_string()).collect(),
            respect_ignore: true,
            max_candidates: 200,
            threads: None,
            prefer_headers: false,
        }
    }
}

/// Result of a candidate search.
#[derive(Clone, Debug)]
pub struct FinderResult {
    pub candidates: Vec<Candidate>,
    /// True if the distinct-file cap was hit and the walk stopped early
    /// (design-specs §10 — surfaced as `truncated` on the wire).
    pub truncated: bool,
}

/// The bare final identifier component of a (possibly) qualified target.
///
/// `A::B::ParseNode` → `ParseNode`, `operator==` → `operator==`, `f` → `f`.
/// Used as the prefilter needle to maximize recall.
fn bare_component(target: &str) -> &str {
    match target.rfind("::") {
        Some(idx) => &target[idx + 2..],
        None => target,
    }
}

/// Build a whole-word matcher for the identifier. Word boundaries avoid matching
/// `f` inside `foo`; the needle is regex-escaped so `operator==` is literal.
fn build_matcher(needle: &str) -> Result<RegexMatcher> {
    let pattern = format!(r"\b{}\b", regex::escape(needle));
    RegexMatcher::new(&pattern)
        .with_context(|| format!("failed to build search matcher for {needle:?}"))
}

/// Sink that collects every matching line in a single file.
struct CollectSink<'a> {
    path: &'a Path,
    hits: Vec<Candidate>,
}

impl Sink for CollectSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, m: &SinkMatch<'_>) -> Result<bool, std::io::Error> {
        let line = m.line_number().unwrap_or(0) as usize;
        let byte_offset = m.absolute_byte_offset() as usize;
        let snippet = String::from_utf8_lossy(m.bytes())
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.hits.push(Candidate {
            file_path: self.path.to_path_buf(),
            line,
            byte_offset,
            snippet,
        });
        Ok(true)
    }
}

/// Does this path's extension pass the configured filter?
fn ext_allowed(path: &Path, extensions: &[String]) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => extensions.iter().any(|allowed| allowed == ext),
        None => false,
    }
}

/// Shared, lock-guarded accumulator across walker threads.
struct Shared {
    candidates: Vec<Candidate>,
    files: HashSet<PathBuf>,
    truncated: bool,
}

/// Find candidate files+lines mentioning `target` under the configured roots.
///
/// When `cfg.prefer_headers` is set (find-decl, design-specs §7.2 step 1), a
/// header-only pass runs first; only an empty header result falls back to a
/// full search over every configured extension.
pub fn find_candidates(target: &str, cfg: &FinderConfig) -> Result<FinderResult> {
    if cfg.prefer_headers {
        let header_exts: Vec<String> = cfg
            .extensions
            .iter()
            .filter(|e| is_header_ext(e))
            .cloned()
            .collect();
        if !header_exts.is_empty() {
            let header_cfg = FinderConfig {
                extensions: header_exts,
                prefer_headers: false,
                ..cfg.clone()
            };
            let res = run_search(target, &header_cfg)?;
            if !res.candidates.is_empty() {
                return Ok(res);
            }
        }
    }
    run_search(target, cfg)
}

/// Header extensions preferred by find-decl's first pass (design-specs §7.2).
const HEADER_EXTENSIONS: &[&str] = &["h", "hpp", "hh", "hxx"];

/// Whether `ext` (no leading dot) is a C/C++ header extension.
fn is_header_ext(ext: &str) -> bool {
    HEADER_EXTENSIONS.contains(&ext)
}

/// Core single-pass candidate search over `cfg.extensions`.
///
/// Uses a parallel, ignore-aware walk; each file is line-searched with grep.
/// Stops early once `max_candidates` distinct files have matched, setting
/// `truncated`. Results are sorted deterministically (file, then byte offset).
fn run_search(target: &str, cfg: &FinderConfig) -> Result<FinderResult> {
    let needle = bare_component(target);
    let matcher = build_matcher(needle)?;

    let roots: Vec<PathBuf> = if cfg.roots.is_empty() {
        vec![PathBuf::from(".")]
    } else {
        cfg.roots.clone()
    };

    let mut builder = WalkBuilder::new(&roots[0]);
    for root in &roots[1..] {
        builder.add(root);
    }
    // standard_filters toggles gitignore/.ignore/hidden/parents in one call.
    builder.standard_filters(cfg.respect_ignore);
    // Apply .gitignore even when the search root is not inside a git work tree
    // (the default `ignore` behavior gates gitignore on git presence).
    builder.require_git(false);
    if let Some(n) = cfg.threads {
        builder.threads(n);
    }

    let shared = Mutex::new(Shared {
        candidates: Vec::new(),
        files: HashSet::new(),
        truncated: false,
    });

    let extensions = &cfg.extensions;
    let max = cfg.max_candidates;
    // Borrow shared, read-only state into the parallel walk. `&T` is `Copy`, so
    // each per-thread visitor closure gets its own copy of the borrow. Scope the
    // borrows so the owned `shared` is free for `into_inner()` afterward.
    {
        let shared = &shared;
        let matcher = &matcher;

        builder.build_parallel().run(|| {
            // One searcher per thread; matcher is shared (read-only).
            let mut searcher = SearcherBuilder::new().line_number(true).build();
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => return WalkState::Continue, // skip unreadable entries
                };
                let path = entry.path();
                if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                    return WalkState::Continue;
                }
                if !ext_allowed(path, extensions) {
                    return WalkState::Continue;
                }

                let mut sink = CollectSink {
                    path,
                    hits: Vec::new(),
                };
                if searcher.search_path(matcher, path, &mut sink).is_err() {
                    return WalkState::Continue; // unreadable/binary: best-effort skip
                }
                if sink.hits.is_empty() {
                    return WalkState::Continue;
                }

                let mut state = shared.lock().unwrap();
                if state.truncated {
                    return WalkState::Quit;
                }
                // Enforce the distinct-file cap; this file is new (one entry/file).
                if state.files.len() >= max {
                    state.truncated = true;
                    return WalkState::Quit;
                }
                state.files.insert(path.to_path_buf());
                state.candidates.extend(sink.hits);
                WalkState::Continue
            })
        });
    }

    let shared = shared.into_inner().unwrap();
    let mut candidates = shared.candidates;
    candidates.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.byte_offset.cmp(&b.byte_offset))
    });

    Ok(FinderResult {
        candidates,
        truncated: shared.truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, name: &str, body: &str) {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn cfg(root: &Path) -> FinderConfig {
        FinderConfig {
            roots: vec![root.to_path_buf()],
            ..Default::default()
        }
    }

    #[test]
    fn prefers_headers_when_available() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.cpp", "int needle;\n");
        write(dir.path(), "a.hpp", "int needle;\n");
        let mut c = cfg(dir.path());
        c.prefer_headers = true;
        let res = find_candidates("needle", &c).unwrap();
        assert_eq!(res.candidates.len(), 1);
        assert!(res.candidates[0].file_path.ends_with("a.hpp"));
    }

    #[test]
    fn falls_back_to_sources_when_no_header() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.cpp", "int needle;\n");
        let mut c = cfg(dir.path());
        c.prefer_headers = true;
        let res = find_candidates("needle", &c).unwrap();
        assert_eq!(res.candidates.len(), 1);
        assert!(res.candidates[0].file_path.ends_with("a.cpp"));
    }

    #[test]
    fn bare_component_strips_qualifiers() {
        assert_eq!(bare_component("A::B::f"), "f");
        assert_eq!(bare_component("f"), "f");
        assert_eq!(bare_component("operator=="), "operator==");
    }

    #[test]
    fn finds_whole_word_only() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.cpp", "int foo = 1;\nint foobar = 2;\nfoo();\n");
        let res = find_candidates("foo", &cfg(dir.path())).unwrap();
        // Lines 1 and 3 mention `foo`; line 2's `foobar` must not match.
        assert_eq!(res.candidates.len(), 2);
        assert_eq!(res.candidates[0].line, 1);
        assert_eq!(res.candidates[1].line, 3);
        assert!(!res.truncated);
    }

    #[test]
    fn qualified_target_matches_bare_use() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.cpp", "void ParseNode() {}\n");
        let res = find_candidates("ast::ParseNode", &cfg(dir.path())).unwrap();
        assert_eq!(res.candidates.len(), 1);
        assert_eq!(res.candidates[0].line, 1);
    }

    #[test]
    fn respects_extension_filter() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.cpp", "int target;\n");
        write(dir.path(), "b.txt", "int target;\n");
        write(dir.path(), "c.py", "target = 1\n");
        let res = find_candidates("target", &cfg(dir.path())).unwrap();
        assert_eq!(res.candidates.len(), 1);
        assert!(res.candidates[0].file_path.ends_with("a.cpp"));
    }

    #[test]
    fn honors_gitignore() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".gitignore", "build/\n");
        write(dir.path(), "src/a.cpp", "int needle;\n");
        write(dir.path(), "build/gen.cpp", "int needle;\n");
        let res = find_candidates("needle", &cfg(dir.path())).unwrap();
        assert_eq!(res.candidates.len(), 1);
        assert!(res.candidates[0].file_path.ends_with("src/a.cpp"));
    }

    #[test]
    fn no_ignore_includes_ignored() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".gitignore", "build/\n");
        write(dir.path(), "src/a.cpp", "int needle;\n");
        write(dir.path(), "build/gen.cpp", "int needle;\n");
        let mut c = cfg(dir.path());
        c.respect_ignore = false;
        let res = find_candidates("needle", &c).unwrap();
        assert_eq!(res.candidates.len(), 2);
    }

    #[test]
    fn caps_distinct_files_and_marks_truncated() {
        let dir = TempDir::new().unwrap();
        for i in 0..10 {
            write(dir.path(), &format!("f{i}.cpp"), "int hit;\n");
        }
        let mut c = cfg(dir.path());
        c.max_candidates = 3;
        let res = find_candidates("hit", &c).unwrap();
        assert!(res.truncated);
        // Distinct files are capped at 3.
        let files: HashSet<_> = res.candidates.iter().map(|c| &c.file_path).collect();
        assert_eq!(files.len(), 3);
    }

    #[test]
    fn not_found_is_empty_not_error() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.cpp", "int other;\n");
        let res = find_candidates("missing", &cfg(dir.path())).unwrap();
        assert!(res.candidates.is_empty());
        assert!(!res.truncated);
    }
}
