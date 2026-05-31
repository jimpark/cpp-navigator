//! Syntactic engine — Stage 1 of the pipeline (design-specs §4, §5).
//!
//! Parses each candidate file with `tree-sitter-cpp` and extracts byte-exact
//! boundaries for the named construct. This is the default, fully self-contained
//! backend: zero build setup, no system libraries. Its limit is *semantic*
//! disambiguation (overload/template selection), which is the opt-in libclang
//! backend's job (Phase 7).
//!
//! Matching is *syntactic*: a node matches if its declarator name equals the
//! target's bare component (and, for a qualified target, the node's qualified
//! name ends with the target). Multiple matches surface as `ambiguous`
//! (design-specs §11), exactly the overload case.

use std::collections::HashSet;
use std::path::Path;

use rayon::prelude::*;
use tree_sitter::{Node, Parser, Tree};

use crate::engine::Engine;
use crate::model::{Kind, Resolution, SourceRef, Span, Status, Symbol};
use crate::search::Candidate;

/// tree-sitter syntactic backend.
pub struct SyntacticEngine;

impl SyntacticEngine {
    pub fn new() -> Self {
        SyntacticEngine
    }
}

impl Default for SyntacticEngine {
    fn default() -> Self {
        Self::new()
    }
}

const ENGINE_NAME: &str = "tree-sitter";
/// Syntactic results are confident on boundaries but not on overload selection.
const SYNTACTIC_CONFIDENCE: f32 = 0.8;

impl Engine for SyntacticEngine {
    fn name(&self) -> &str {
        ENGINE_NAME
    }

    fn definitions(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution> {
        run_over_files(target, candidates, Mode::Definition)
    }

    fn declarations(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution> {
        run_over_files(target, candidates, Mode::Declaration)
    }

    fn enclosing_scope(&self, file: &Path, byte_offset: usize) -> Option<Span> {
        let src = std::fs::read(file).ok()?;
        let tree = parse(&src)?;
        let root = tree.root_node();
        let node = root.descendant_for_byte_range(byte_offset, byte_offset)?;
        // Walk up to the nearest function/template enclosure.
        let mut cur = Some(node);
        while let Some(n) = cur {
            if matches!(
                n.kind(),
                "function_definition" | "template_declaration"
            ) {
                return Some(span_of(n));
            }
            cur = n.parent();
        }
        None
    }

    fn enclosing_class_scope(&self, file: &Path, byte_offset: usize) -> Option<Span> {
        let src = std::fs::read(file).ok()?;
        let tree = parse(&src)?;
        let root = tree.root_node();
        let node = root.descendant_for_byte_range(byte_offset, byte_offset)?;
        // Walk up to the nearest class/struct definition. Expand to a wrapping
        // `template_declaration` so a templated class includes its `template<...>`
        // prefix (see `report_node`).
        let mut cur = Some(node);
        while let Some(n) = cur {
            if matches!(n.kind(), "class_specifier" | "struct_specifier") {
                return Some(span_of(report_node(n)));
            }
            cur = n.parent();
        }
        None
    }
}

/// Whether we are collecting definitions (with a body/initializer) or
/// declarations (signature only).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Definition,
    Declaration,
}

/// Parse a C++ source buffer, returning its syntax tree.
fn parse(src: &[u8]) -> Option<Tree> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .ok()?;
    parser.parse(src, None)
}

/// Parse every distinct candidate file (in parallel) and merge matches.
///
/// The candidate line is only a prefilter hint; we search the whole parsed file
/// for the name. Results are deduplicated by exact span and ordered
/// deterministically (file path, then start byte) for stable output.
fn run_over_files(target: &str, candidates: &[Candidate], mode: Mode) -> Vec<Resolution> {
    // Distinct files, preserving the finder's deterministic order.
    let mut seen = HashSet::new();
    let files: Vec<&Path> = candidates
        .iter()
        .filter(|c| seen.insert(c.file_path.as_path()))
        .map(|c| c.file_path.as_path())
        .collect();

    let mut resolutions: Vec<Resolution> = files
        .par_iter()
        .flat_map_iter(|path| matches_in_file(path, target, mode))
        .collect();

    resolutions.sort_by(|a, b| {
        a.source_ref
            .file_path
            .cmp(&b.source_ref.file_path)
            .then(a.source_ref.span.start_byte.cmp(&b.source_ref.span.start_byte))
    });
    resolutions.dedup_by(|a, b| {
        a.source_ref.file_path == b.source_ref.file_path
            && a.source_ref.span.start_byte == b.source_ref.span.start_byte
            && a.source_ref.span.end_byte == b.source_ref.span.end_byte
    });
    resolutions
}

/// Collect all matching resolutions within a single file.
fn matches_in_file(path: &Path, target: &str, mode: Mode) -> Vec<Resolution> {
    let src = match std::fs::read(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let tree = match parse(&src) {
        Some(t) => t,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    walk(tree.root_node(), &src, target, mode, path, &mut out);
    out
}

/// Recursive tree walk collecting matches.
fn walk(
    node: Node,
    src: &[u8],
    target: &str,
    mode: Mode,
    path: &Path,
    out: &mut Vec<Resolution>,
) {
    if let Some(res) = try_match(node, src, target, mode, path) {
        out.push(res);
    }
    // Recurse into children. We descend unconditionally; template-wrapped
    // definitions are matched at the inner node and expanded to the template
    // span (see `report_node`), so there is no double counting.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, src, target, mode, path, out);
    }
}

/// Attempt to match a single node against the target in the given mode.
fn try_match(
    node: Node,
    src: &[u8],
    target: &str,
    mode: Mode,
    path: &Path,
) -> Option<Resolution> {
    let kind = classify(node, mode)?;
    let decl = name_node(node)?;
    let (bare, qualified) = name_text(decl, src)?;

    if !name_matches(target, &bare, qualified.as_deref()) {
        return None;
    }

    // Expand template-wrapped constructs to the full template_declaration span.
    let report = report_node(node);
    let span = span_of(report);
    let content_bytes = src[span.start_byte..span.end_byte].to_vec();

    let (signature, type_spelling, doc) = if mode == Mode::Declaration {
        (
            Some(text(node, src).trim().to_string()),
            type_spelling(node, src),
            leading_doc(report, src),
        )
    } else {
        (None, None, None)
    };

    let resolution_kind = if report.kind() == "template_declaration" {
        Kind::Template
    } else {
        kind
    };

    Some(Resolution {
        symbol: Symbol {
            name: bare,
            qualified_name: qualified,
            kind: resolution_kind,
            signature,
            type_spelling,
            doc,
        },
        source_ref: SourceRef {
            file_path: path.to_path_buf(),
            span,
        },
        content_bytes,
        engine: ENGINE_NAME.to_string(),
        confidence: SYNTACTIC_CONFIDENCE,
        status: Status::Resolved,
    })
}

/// Classify a node as a definition or declaration of a given kind, or `None`
/// if it is neither (for the requested mode).
fn classify(node: Node, mode: Mode) -> Option<Kind> {
    match node.kind() {
        "function_definition" => (mode == Mode::Definition).then_some(Kind::Function),
        "class_specifier" => {
            (mode == Mode::Definition && node.child_by_field_name("body").is_some())
                .then_some(Kind::Class)
        }
        "struct_specifier" => {
            (mode == Mode::Definition && node.child_by_field_name("body").is_some())
                .then_some(Kind::Struct)
        }
        "declaration" => {
            let has_init = has_initializer(node);
            let is_proto = is_function_prototype(node);
            match mode {
                // Variable definition: a declaration with an initializer (and
                // not a function prototype).
                Mode::Definition if has_init && !is_proto => Some(Kind::Variable),
                // Declaration: a function prototype, or a variable declaration
                // with no initializer.
                Mode::Declaration if is_proto => Some(Kind::Function),
                Mode::Declaration if !has_init => Some(Kind::Variable),
                _ => None,
            }
        }
        // Class members (method prototypes, member variables).
        "field_declaration" => (mode == Mode::Declaration).then_some(Kind::Member),
        _ => None,
    }
}

/// Does a `declaration` node contain an `init_declarator` with a value?
fn has_initializer(node: Node) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| {
        c.kind() == "init_declarator" && c.child_by_field_name("value").is_some()
    })
}

/// Does a `declaration`/`field_declaration` declare a function (prototype)?
fn is_function_prototype(node: Node) -> bool {
    let Some(decl) = node.child_by_field_name("declarator") else {
        return false;
    };
    // Peel pointer/reference wrappers down to a function_declarator.
    let mut cur = decl;
    loop {
        match cur.kind() {
            "function_declarator" => return true,
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                match cur.child_by_field_name("declarator") {
                    Some(inner) => cur = inner,
                    None => return false,
                }
            }
            _ => return false,
        }
    }
}

/// The node whose span we report — expands a definition/declaration to the
/// enclosing `template_declaration` so the full `template<...>` prefix is
/// included (design-specs §11).
fn report_node(node: Node) -> Node {
    if let Some(parent) = node.parent()
        && parent.kind() == "template_declaration"
    {
        return parent;
    }
    node
}

/// Locate the name (declarator) node for a definition/declaration node.
fn name_node(node: Node) -> Option<Node> {
    match node.kind() {
        "class_specifier" | "struct_specifier" => node.child_by_field_name("name"),
        _ => {
            let decl = node.child_by_field_name("declarator")?;
            innermost_name(decl)
        }
    }
}

/// Descend through declarator wrappers to the terminal name node.
fn innermost_name(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" | "qualified_identifier"
        | "destructor_name" | "operator_name" => Some(node),
        "pointer_declarator" | "reference_declarator" | "array_declarator"
        | "parenthesized_declarator" | "init_declarator" | "function_declarator" => {
            let inner = node.child_by_field_name("declarator")?;
            innermost_name(inner)
        }
        _ => node.child_by_field_name("declarator").and_then(innermost_name),
    }
}

/// Extract `(bare, qualified)` names from a name node.
/// `A::B::f` → ("f", Some("A::B::f")); `f` → ("f", None).
fn name_text(node: Node, src: &[u8]) -> Option<(String, Option<String>)> {
    let full = text(node, src);
    if full.is_empty() {
        return None;
    }
    if node.kind() == "qualified_identifier" {
        let bare = full.rsplit("::").next().unwrap_or(&full).to_string();
        Some((bare, Some(full)))
    } else {
        Some((full, None))
    }
}

/// Does `target` match a node's bare/qualified name? A bare target matches by
/// final component; a qualified target additionally requires the node's
/// qualified name to end with the target.
fn name_matches(target: &str, bare: &str, qualified: Option<&str>) -> bool {
    let target_bare = target.rsplit("::").next().unwrap_or(target);
    if bare != target_bare {
        return false;
    }
    if target.contains("::") {
        match qualified {
            Some(q) => q == target || q.ends_with(&format!("::{target}")) || q.ends_with(target),
            None => false,
        }
    } else {
        true
    }
}

/// Best-effort type spelling for `find-decl` (design-specs §8.3). For functions:
/// `ret(param_types...)`; for variables: the declared type text. Omitted when it
/// cannot be derived confidently.
fn type_spelling(node: Node, src: &[u8]) -> Option<String> {
    let ret = node.child_by_field_name("type").map(|t| text(t, src))?;
    let decl = node.child_by_field_name("declarator")?;
    if let Some(func) = find_function_declarator(decl) {
        let params = func.child_by_field_name("parameters")?;
        let mut cursor = params.walk();
        let types: Vec<String> = params
            .children(&mut cursor)
            .filter(|c| c.kind() == "parameter_declaration")
            .filter_map(|p| p.child_by_field_name("type").map(|t| text(t, src)))
            .collect();
        Some(format!("{}({})", ret.trim(), types.join(", ")))
    } else {
        Some(ret.trim().to_string())
    }
}

/// Find a `function_declarator` by peeling pointer/reference wrappers.
fn find_function_declarator(node: Node) -> Option<Node> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "function_declarator" => return Some(cur),
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator"
            | "init_declarator" => {
                cur = cur.child_by_field_name("declarator")?;
            }
            _ => return None,
        }
    }
}

/// Collect the contiguous comment block immediately above `node` (design-specs
/// §7.2: line `//` runs or `/* ... */` blocks). Returns `None` if absent.
fn leading_doc(node: Node, src: &[u8]) -> Option<String> {
    let mut comments: Vec<String> = Vec::new();
    let mut anchor_row = node.start_position().row;
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() != "comment" {
            break;
        }
        // Require adjacency: the comment must sit directly above (no blank gap).
        if p.end_position().row + 1 < anchor_row {
            break;
        }
        comments.push(text(p, src));
        anchor_row = p.start_position().row;
        prev = p.prev_sibling();
    }
    if comments.is_empty() {
        None
    } else {
        comments.reverse();
        Some(comments.join("\n"))
    }
}

/// Verbatim text of a node.
fn text(node: Node, src: &[u8]) -> String {
    String::from_utf8_lossy(&src[node.start_byte()..node.end_byte()]).into_owned()
}

/// Byte/line/col span of a node (lines are 1-based; cols are 0-based bytes).
fn span_of(node: Node) -> Span {
    let start = node.start_position();
    let end = node.end_position();
    Span {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: start.row + 1,
        end_line: end.row + 1,
        start_col: start.column,
        end_col: end.column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn candidates_for(path: &Path) -> Vec<Candidate> {
        vec![Candidate {
            file_path: path.to_path_buf(),
            line: 1,
            byte_offset: 0,
            snippet: String::new(),
        }]
    }

    fn write(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn finds_function_definition_with_exact_bytes() {
        let dir = TempDir::new().unwrap();
        let body = "int add(int a, int b) {\n    return a + b;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.definitions("add", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let r = &res[0];
        // Byte-fidelity: re-slice from disk equals reported content.
        let disk = fs::read(&p).unwrap();
        let slice = &disk[r.source_ref.span.start_byte..r.source_ref.span.end_byte];
        assert_eq!(slice, r.content_bytes.as_slice());
        assert_eq!(r.content_bytes, b"int add(int a, int b) {\n    return a + b;\n}");
        assert_eq!(r.symbol.kind, Kind::Function);
    }

    #[test]
    fn overloads_yield_multiple_matches() {
        let dir = TempDir::new().unwrap();
        let body = "void f(int a) {}\nvoid f(double a) {}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.definitions("f", &candidates_for(&p));
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn template_span_includes_prefix() {
        let dir = TempDir::new().unwrap();
        let body = "template <typename T>\nT identity(T x) {\n    return x;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.definitions("identity", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].symbol.kind, Kind::Template);
        // Span starts at the `template` keyword, not at `T identity`.
        assert_eq!(res[0].source_ref.span.start_line, 1);
        assert!(String::from_utf8_lossy(&res[0].content_bytes).starts_with("template"));
    }

    #[test]
    fn qualified_target_matches_out_of_line_def() {
        let dir = TempDir::new().unwrap();
        let body = "void Foo::bar() {\n    return;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        assert_eq!(eng.definitions("Foo::bar", &candidates_for(&p)).len(), 1);
        assert_eq!(eng.definitions("bar", &candidates_for(&p)).len(), 1);
        // A wrong qualifier must not match.
        assert_eq!(eng.definitions("Baz::bar", &candidates_for(&p)).len(), 0);
    }

    #[test]
    fn declaration_carries_signature_and_doc() {
        let dir = TempDir::new().unwrap();
        let body = "/// Allocate the global pool.\nvoid InitPool(size_t n);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("InitPool", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let s = &res[0].symbol;
        assert_eq!(s.signature.as_deref(), Some("void InitPool(size_t n);"));
        assert_eq!(s.doc.as_deref(), Some("/// Allocate the global pool."));
        assert_eq!(s.type_spelling.as_deref(), Some("void(size_t)"));
    }

    #[test]
    fn definition_is_not_returned_as_declaration() {
        let dir = TempDir::new().unwrap();
        let body = "int add(int a) { return a; }\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        // It's a definition, so find-decl (declarations) should not match it.
        assert_eq!(eng.declarations("add", &candidates_for(&p)).len(), 0);
        assert_eq!(eng.definitions("add", &candidates_for(&p)).len(), 1);
    }

    #[test]
    fn variable_with_initializer_is_a_definition() {
        let dir = TempDir::new().unwrap();
        let body = "int counter = 42;\nextern int other;\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let defs = eng.definitions("counter", &candidates_for(&p));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].symbol.kind, Kind::Variable);
        // `other` has no initializer → declaration, not definition.
        assert_eq!(eng.definitions("other", &candidates_for(&p)).len(), 0);
        assert_eq!(eng.declarations("other", &candidates_for(&p)).len(), 1);
    }

    #[test]
    fn enclosing_scope_finds_function() {
        let dir = TempDir::new().unwrap();
        let body = "int add(int a, int b) {\n    return a + b;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        // Byte offset inside `return a + b;`.
        let off = body.find("return").unwrap();
        let span = eng.enclosing_scope(&p, off).unwrap();
        assert_eq!(span.start_line, 1);
        assert_eq!(span.end_line, 3);
    }

    #[test]
    fn enclosing_class_scope_covers_inline_member() {
        let dir = TempDir::new().unwrap();
        let body = "class Widget {\npublic:\n    int area() { return w * h; }\n    int w, h;\n};\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        // Offset inside the inline method body.
        let off = body.find("return w * h").unwrap();
        let span = eng.enclosing_class_scope(&p, off).unwrap();
        // Spans the whole `class Widget { ... }` (excluding the trailing `;`).
        let disk = fs::read(&p).unwrap();
        let slice = &disk[span.start_byte..span.end_byte];
        assert!(slice.starts_with(b"class Widget {"));
        assert!(slice.ends_with(b"}"));
        assert_eq!(span.start_line, 1);
        assert_eq!(span.end_line, 5);
    }

    #[test]
    fn enclosing_class_scope_includes_template_prefix() {
        let dir = TempDir::new().unwrap();
        let body =
            "template <typename T>\nclass Box {\npublic:\n    T get() { return v; }\n    T v;\n};\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let off = body.find("return v").unwrap();
        let span = eng.enclosing_class_scope(&p, off).unwrap();
        let disk = fs::read(&p).unwrap();
        let slice = &disk[span.start_byte..span.end_byte];
        // The reported span starts at the `template` keyword.
        assert!(slice.starts_with(b"template <typename T>"));
        assert_eq!(span.start_line, 1);
    }

    #[test]
    fn enclosing_class_scope_none_for_out_of_line_member() {
        let dir = TempDir::new().unwrap();
        // Out-of-line member: lexical encloser is the TU, not the class.
        let body = "void Foo::bar() {\n    return;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let off = body.find("return").unwrap();
        assert!(eng.enclosing_class_scope(&p, off).is_none());
    }

    #[test]
    fn enclosing_class_scope_none_for_free_function() {
        let dir = TempDir::new().unwrap();
        let body = "int add(int a, int b) {\n    return a + b;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let off = body.find("return").unwrap();
        assert!(eng.enclosing_class_scope(&p, off).is_none());
    }
}
