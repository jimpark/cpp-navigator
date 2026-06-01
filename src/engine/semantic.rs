//! Semantic engine — Stage 2 of the pipeline (design-specs §4, §5).
//!
//! Uses libclang (via the `clang` crate) to resolve symbols with full
//! overload/template/namespace semantics. Requires a `compile_commands.json`
//! for accurate flag pass-through. Falls back to Stage 1 (tree-sitter) when
//! the compilation database is missing or parsing fails.
//!
//! Gated behind `#[cfg(feature = "semantic")]`.

use std::path::{Path, PathBuf};

use clang::{Clang, CompilationDatabase, Entity, EntityKind, EntityVisitResult, Index};

use crate::engine::Engine;
use crate::model::{Kind, Resolution, SourceRef, Span, Status, Symbol};
use crate::search::Candidate;

const ENGINE_NAME: &str = "libclang";
const SEMANTIC_CONFIDENCE: f32 = 0.95;

/// libclang semantic backend.
pub struct SemanticEngine {
    /// Path to compile_commands.json (or directory containing it).
    compile_db_path: PathBuf,
}

impl SemanticEngine {
    /// Create a new semantic engine with the given compile_commands.json path.
    ///
    /// `path` can be the file itself or the directory containing it.
    pub fn new(path: PathBuf) -> Self {
        SemanticEngine {
            compile_db_path: path,
        }
    }

    /// Load the compilation database. Returns `None` if it can't be loaded.
    fn load_db(&self) -> Option<CompilationDatabase> {
        let dir = if self.compile_db_path.is_file() {
            self.compile_db_path.parent()?.to_path_buf()
        } else {
            self.compile_db_path.clone()
        };
        CompilationDatabase::from_directory(&dir).ok()
    }

    /// Get compile arguments for a specific file from the compilation database.
    fn get_arguments(&self, db: &CompilationDatabase, file: &Path) -> Vec<String> {
        match db.get_compile_commands(file) {
            Ok(commands) => {
                // Use the first command's arguments.
                let cmds = commands.get_commands();
                if let Some(cmd) = cmds.first() {
                    let args = cmd.get_arguments();
                    // Filter out the source file itself, -c, -o and its argument.
                    let mut result = Vec::new();
                    let mut skip_next = false;
                    for arg in &args[1..] {
                        // skip 0 = compiler exe
                        if skip_next {
                            skip_next = false;
                            continue;
                        }
                        if arg == "-c" || arg == "-emit-ast" || arg == "-fsyntax-only" {
                            continue;
                        }
                        if arg == "-o" {
                            skip_next = true;
                            continue;
                        }
                        if arg.starts_with("-o") {
                            continue;
                        }
                        // Skip the source file itself.
                        if Path::new(arg) == file {
                            continue;
                        }
                        result.push(arg.clone());
                    }
                    result
                } else {
                    Vec::new()
                }
            }
            Err(_) => Vec::new(),
        }
    }

    /// Parse a file with libclang and find all entities matching `target`.
    fn find_entities_in_file(
        &self,
        target: &str,
        file: &Path,
        args: &[String],
        mode: SemanticMode,
    ) -> Vec<Resolution> {
        let clang = match Clang::new() {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };
        let index = Index::new(&clang, false, false);
        let mut parser = index.parser(file);
        let str_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        parser.arguments(&str_args);

        let tu = match parser.parse() {
            Ok(tu) => tu,
            Err(_) => return Vec::new(),
        };

        let root = tu.get_entity();
        let target_bare = target.rsplit("::").next().unwrap_or(target);
        let mut results = Vec::new();

        collect_matches(&root, target, target_bare, mode, file, &mut results);
        results
    }
}

/// Whether we are looking for definitions or declarations.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SemanticMode {
    Definition,
    Declaration,
}

/// Recursively collect matching entities from the AST.
fn collect_matches(
    entity: &Entity,
    target: &str,
    target_bare: &str,
    mode: SemanticMode,
    file: &Path,
    results: &mut Vec<Resolution>,
) {
    let kind = entity.get_kind();
    let name = entity.get_name().unwrap_or_default();

    // Check if this entity's name matches our target.
    if (name == target_bare || matches_qualified(entity, target))
        && let Some(resolution) = try_resolve_entity(entity, kind, mode, file)
    {
        // Deduplicate by location.
        let loc_key = (
            resolution.source_ref.file_path.clone(),
            resolution.source_ref.span.start_byte,
        );
        if !results
            .iter()
            .any(|r| r.source_ref.file_path == loc_key.0 && r.source_ref.span.start_byte == loc_key.1)
        {
            results.push(resolution);
        }
    }

    // Recurse into children.
    entity.visit_children(|child, _| {
        collect_matches(&child, target, target_bare, mode, file, results);
        EntityVisitResult::Continue
    });
}

/// Check if an entity matches a qualified target like `Foo::bar`.
fn matches_qualified(entity: &Entity, target: &str) -> bool {
    if !target.contains("::") {
        return false;
    }
    // Build the qualified name from the entity's semantic parents.
    let mut parts = Vec::new();
    if let Some(name) = entity.get_name() {
        parts.push(name);
    } else {
        return false;
    }
    let mut parent = entity.get_semantic_parent();
    while let Some(p) = parent {
        match p.get_kind() {
            EntityKind::Namespace
            | EntityKind::ClassDecl
            | EntityKind::StructDecl
            | EntityKind::ClassTemplate => {
                if let Some(name) = p.get_name() {
                    parts.push(name);
                }
            }
            EntityKind::TranslationUnit => break,
            _ => {}
        }
        parent = p.get_semantic_parent();
    }
    parts.reverse();
    let qualified = parts.join("::");
    qualified == target || qualified.ends_with(&format!("::{target}"))
}

/// Try to resolve a matching entity into a Resolution.
fn try_resolve_entity(
    entity: &Entity,
    kind: EntityKind,
    mode: SemanticMode,
    _origin_file: &Path,
) -> Option<Resolution> {
    let is_def = entity.is_definition();

    match mode {
        SemanticMode::Definition if !is_def => return None,
        SemanticMode::Declaration if is_def => return None,
        _ => {}
    }

    let location = entity.get_location()?;
    let loc = location.get_file_location();
    let file_ref = loc.file?;
    let file_path = file_ref.get_path();
    let start_line = loc.line;
    let start_col = loc.column;

    // Only include results from the file we're analyzing (or its headers).
    let range = entity.get_range()?;
    let end_loc = range.get_end().get_file_location();
    let end_line = end_loc.line;
    let end_col = end_loc.column;

    // Read the source to get byte offsets.
    let src = std::fs::read(&file_path).ok()?;
    let start_byte = line_col_to_byte(&src, start_line as usize, start_col as usize)?;
    let end_byte = line_col_to_byte(&src, end_line as usize, end_col as usize)?;

    if start_byte >= end_byte || end_byte > src.len() {
        return None;
    }

    let model_kind = entity_kind_to_model_kind(kind, mode);
    let name = entity.get_name().unwrap_or_default();
    let qualified_name = build_qualified_name(entity);

    // Extend the reported span backward to include any leading doc comment so
    // that `content` carries the full context, matching the fallback path.
    let doc_comment = entity.get_comment();
    let (content_start_byte, content_start_line) =
        leading_comment_start(&src, start_line as usize)
            .unwrap_or((start_byte, start_line as usize));

    let content_bytes = src[content_start_byte..end_byte].to_vec();

    let (signature, type_spelling) = if mode == SemanticMode::Declaration {
        let sig = entity.get_display_name();
        let type_str = entity.get_type().map(|t| t.get_display_name());
        (sig, type_str)
    } else {
        (None, None)
    };

    Some(Resolution {
        symbol: Symbol {
            name,
            qualified_name,
            kind: model_kind,
            signature,
            type_spelling,
            doc: doc_comment,
        },
        source_ref: SourceRef {
            file_path,
            span: Span {
                start_byte: content_start_byte,
                end_byte,
                start_line: content_start_line,
                end_line: end_line as usize,
                start_col: 0,
                end_col: end_col as usize,
            },
        },
        content_bytes,
        engine: ENGINE_NAME.to_string(),
        confidence: SEMANTIC_CONFIDENCE,
        status: Status::Resolved,
    })
}

/// Scan `src` backward from `decl_start_line` (1-based) to find the start of a
/// contiguous C/C++ comment block (`//…` or `/* … */`) that sits immediately
/// above the declaration with no blank lines. Returns `(start_byte, start_line)`
/// of the topmost comment line, or `None` when no adjacent comment exists.
fn leading_comment_start(src: &[u8], decl_start_line: usize) -> Option<(usize, usize)> {
    // Split into lines, keeping their byte offsets.
    let mut line_starts: Vec<usize> = vec![0];
    for (i, &b) in src.iter().enumerate() {
        if b == b'\n' && i + 1 < src.len() {
            line_starts.push(i + 1);
        }
    }
    if decl_start_line == 0 || decl_start_line > line_starts.len() {
        return None;
    }
    // Walk upward from the line immediately above the declaration.
    let mut comment_start: Option<(usize, usize)> = None;
    let mut expected_next = decl_start_line - 1; // 1-based line we're examining
    loop {
        if expected_next == 0 {
            break;
        }
        let line_idx = expected_next - 1; // 0-based index into line_starts
        let line_start = line_starts[line_idx];
        let line_end = if line_idx + 1 < line_starts.len() {
            line_starts[line_idx + 1]
        } else {
            src.len()
        };
        let line = std::str::from_utf8(&src[line_start..line_end])
            .unwrap_or("")
            .trim_end_matches(['\n', '\r']);
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*') {
            comment_start = Some((line_start, expected_next));
            expected_next -= 1;
        } else {
            break; // blank line or code — stop
        }
    }
    comment_start
}

/// Convert line:col (1-based) to byte offset.
fn line_col_to_byte(src: &[u8], line: usize, col: usize) -> Option<usize> {
    if line == 0 {
        return None;
    }
    let mut current_line = 1;
    let mut line_start = 0;
    for (i, &b) in src.iter().enumerate() {
        if current_line == line {
            let byte_offset = line_start + col.saturating_sub(1);
            return Some(byte_offset.min(src.len()));
        }
        if b == b'\n' {
            current_line += 1;
            line_start = i + 1;
        }
    }
    if current_line == line {
        Some(line_start + col.saturating_sub(1))
    } else {
        None
    }
}

/// Build the fully qualified name of an entity.
fn build_qualified_name(entity: &Entity) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(name) = entity.get_name() {
        parts.push(name);
    } else {
        return None;
    }
    let mut parent = entity.get_semantic_parent();
    while let Some(p) = parent {
        match p.get_kind() {
            EntityKind::Namespace
            | EntityKind::ClassDecl
            | EntityKind::StructDecl
            | EntityKind::ClassTemplate => {
                if let Some(name) = p.get_name() {
                    parts.push(name);
                }
            }
            EntityKind::TranslationUnit => break,
            _ => {}
        }
        parent = p.get_semantic_parent();
    }
    if parts.len() > 1 {
        parts.reverse();
        Some(parts.join("::"))
    } else {
        None
    }
}

/// Map libclang entity kinds to the internal model Kind.
fn entity_kind_to_model_kind(kind: EntityKind, mode: SemanticMode) -> Kind {
    match kind {
        EntityKind::FunctionDecl => Kind::Function,
        EntityKind::Method | EntityKind::Constructor | EntityKind::Destructor => Kind::Method,
        EntityKind::VarDecl => Kind::Variable,
        EntityKind::FieldDecl => Kind::Member,
        EntityKind::ClassDecl => Kind::Class,
        EntityKind::StructDecl => Kind::Struct,
        EntityKind::FunctionTemplate | EntityKind::ClassTemplate => Kind::Template,
        EntityKind::MacroDefinition => Kind::Macro,
        _ => {
            if mode == SemanticMode::Declaration {
                Kind::Function
            } else {
                Kind::Variable
            }
        }
    }
}

impl Engine for SemanticEngine {
    fn name(&self) -> &str {
        ENGINE_NAME
    }

    fn definitions(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution> {
        let db = match self.load_db() {
            Some(db) => db,
            None => return Vec::new(),
        };

        let mut seen_files = std::collections::HashSet::new();
        let mut all_results = Vec::new();

        for candidate in candidates {
            if !seen_files.insert(candidate.file_path.clone()) {
                continue;
            }
            let args = self.get_arguments(&db, &candidate.file_path);
            let results = self.find_entities_in_file(
                target,
                &candidate.file_path,
                &args,
                SemanticMode::Definition,
            );
            all_results.extend(results);
        }

        // Sort deterministically.
        all_results.sort_by(|a, b| {
            a.source_ref
                .file_path
                .cmp(&b.source_ref.file_path)
                .then(a.source_ref.span.start_byte.cmp(&b.source_ref.span.start_byte))
        });
        all_results
    }

    fn declarations(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution> {
        let db = match self.load_db() {
            Some(db) => db,
            None => return Vec::new(),
        };

        let mut seen_files = std::collections::HashSet::new();
        let mut all_results = Vec::new();

        for candidate in candidates {
            if !seen_files.insert(candidate.file_path.clone()) {
                continue;
            }
            let args = self.get_arguments(&db, &candidate.file_path);
            let results = self.find_entities_in_file(
                target,
                &candidate.file_path,
                &args,
                SemanticMode::Declaration,
            );
            all_results.extend(results);
        }

        all_results.sort_by(|a, b| {
            a.source_ref
                .file_path
                .cmp(&b.source_ref.file_path)
                .then(a.source_ref.span.start_byte.cmp(&b.source_ref.span.start_byte))
        });
        all_results
    }

    fn enclosing_scope(&self, file: &Path, byte_offset: usize) -> Option<Span> {
        // The semantic engine delegates scope queries to tree-sitter since
        // libclang's cursor-based API is less ergonomic for this. The syntactic
        // engine handles this well already.
        let syntactic = crate::engine::SyntacticEngine::new();
        syntactic.enclosing_scope(file, byte_offset)
    }

    fn enclosing_class_scope(&self, file: &Path, byte_offset: usize) -> Option<Span> {
        let syntactic = crate::engine::SyntacticEngine::new();
        syntactic.enclosing_class_scope(file, byte_offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn line_col_to_byte_basic() {
        let src = b"line1\nline2\nline3\n";
        assert_eq!(line_col_to_byte(src, 1, 1), Some(0));
        assert_eq!(line_col_to_byte(src, 1, 3), Some(2));
        assert_eq!(line_col_to_byte(src, 2, 1), Some(6));
        assert_eq!(line_col_to_byte(src, 3, 1), Some(12));
    }

    #[test]
    fn semantic_engine_no_compile_db_returns_empty() {
        let dir = TempDir::new().unwrap();
        let engine = SemanticEngine::new(dir.path().to_path_buf());
        let p = dir.path().join("a.cpp");
        fs::write(&p, "int add(int a, int b) { return a + b; }\n").unwrap();

        let candidates = vec![Candidate {
            file_path: p,
            line: 1,
            byte_offset: 0,
            snippet: String::new(),
        }];
        // No compile_commands.json → empty results (graceful degradation).
        let results = engine.definitions("add", &candidates);
        assert!(results.is_empty());
    }

    #[test]
    fn semantic_engine_with_compile_db() {
        let dir = TempDir::new().unwrap();
        let src_path = dir.path().join("test.cpp");
        fs::write(&src_path, "int add(int a, int b) { return a + b; }\n").unwrap();

        // Create a minimal compile_commands.json.
        let compile_commands = format!(
            r#"[{{"directory": "{dir}","command": "c++ -c test.cpp","file": "{file}"}}]"#,
            dir = dir.path().display(),
            file = src_path.display()
        );
        fs::write(dir.path().join("compile_commands.json"), &compile_commands).unwrap();

        let engine = SemanticEngine::new(dir.path().to_path_buf());
        let candidates = vec![Candidate {
            file_path: src_path,
            line: 1,
            byte_offset: 0,
            snippet: String::new(),
        }];
        let results = engine.definitions("add", &candidates);
        // With a valid compile DB, libclang should find the definition.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].symbol.name, "add");
        assert_eq!(results[0].symbol.kind, Kind::Function);
        assert_eq!(results[0].engine, "libclang");
    }
}
