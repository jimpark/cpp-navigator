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

use crate::engine::macros;
use crate::engine::Engine;
use crate::model::{Kind, Resolution, SourceRef, Span, Status, Symbol};
use crate::search::Candidate;

/// tree-sitter syntactic backend.
pub struct SyntacticEngine {
    /// Macros blanked at every occurrence (`--empty-macro`): full `-DNAME=`
    /// semantics; trusted because the user named them explicitly.
    global_macros: HashSet<String>,
    /// Macros blanked only in annotation position (`TYPE MACRO NAME (`).
    /// Confirmed via project-wide `#define` discovery (plus each file's own
    /// `#define`s, harvested at parse time). See [`macros`].
    annotation_macros: HashSet<String>,
}

impl SyntacticEngine {
    pub fn new() -> Self {
        SyntacticEngine {
            global_macros: HashSet::new(),
            annotation_macros: HashSet::new(),
        }
    }

    /// Build an engine that blanks the given user macro names everywhere (and,
    /// in annotation position, treats them as confirmed). Convenience for the
    /// `--empty-macro`-only path and tests.
    pub fn with_empty_macros<I>(names: I) -> Self
    where
        I: IntoIterator<Item = String>,
    {
        let global: HashSet<String> = names.into_iter().collect();
        SyntacticEngine {
            annotation_macros: global.clone(),
            global_macros: global,
        }
    }

    /// Build an engine with explicit `global` (blank-everywhere) macros and
    /// `discovered` annotation macros (blanked only in annotation position).
    /// The annotation set is the union of both so user macros are also confirmed.
    pub fn with_macros<G, D>(global: G, discovered: D) -> Self
    where
        G: IntoIterator<Item = String>,
        D: IntoIterator<Item = String>,
    {
        let global: HashSet<String> = global.into_iter().collect();
        let mut annotation: HashSet<String> = discovered.into_iter().collect();
        annotation.extend(global.iter().cloned());
        SyntacticEngine {
            global_macros: global,
            annotation_macros: annotation,
        }
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
        run_over_files(target, candidates, Mode::Definition, self)
    }

    fn declarations(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution> {
        run_over_files(target, candidates, Mode::Declaration, self)
    }

    fn enclosing_scope(&self, file: &Path, byte_offset: usize) -> Option<Span> {
        let src = std::fs::read(file).ok()?;
        let tree = parse_recover(&src, self)?;
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
        let tree = parse_recover(&src, self)?;
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

/// Blank confirmed macros in `src` for `eng`, harvesting this file's own
/// `#define`s into the annotation set first. Length-preserving; `None` if
/// nothing was blanked.
fn neutralize_for(eng: &SyntacticEngine, src: &[u8]) -> Option<Vec<u8>> {
    let mut annotation = eng.annotation_macros.clone();
    macros::collect_defines(src, &mut annotation);
    macros::neutralize(src, &eng.global_macros, &annotation)
}

/// Parse `src`, recovering from annotation-macro damage when present.
///
/// If the first parse has errors and blanking confirmed macros yields a strictly
/// cleaner parse, the recovered tree is returned instead. Blanking is
/// length-preserving (see [`macros`]), so the returned tree's byte offsets still
/// index the *original* `src` — callers must continue slicing `src`, never the
/// blanked buffer. Used for single-result lookups (enclosing scopes); the match
/// walk replaces with the cleaner parse instead (see [`matches_in_file`]).
fn parse_recover(src: &[u8], eng: &SyntacticEngine) -> Option<Tree> {
    let tree = parse(src)?;
    if !tree.root_node().has_error() {
        return Some(tree);
    }
    if let Some(neutralized) = neutralize_for(eng, src)
        && let Some(recovered) = parse(&neutralized)
        && error_count(recovered.root_node()) < error_count(tree.root_node())
    {
        return Some(recovered);
    }
    Some(tree)
}

/// Count `ERROR`/`MISSING` nodes in a subtree. Only called on trees already
/// flagged `has_error()`, so the full walk is rare.
fn error_count(node: Node) -> usize {
    let mut n = usize::from(node.is_error() || node.is_missing());
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        n += error_count(child);
    }
    n
}

/// Parse every distinct candidate file (in parallel) and merge matches.
///
/// The candidate line is only a prefilter hint; we search the whole parsed file
/// for the name. Results are deduplicated by exact span and ordered
/// deterministically (file path, then start byte) for stable output.
fn run_over_files(
    target: &str,
    candidates: &[Candidate],
    mode: Mode,
    eng: &SyntacticEngine,
) -> Vec<Resolution> {
    // Distinct files, preserving the finder's deterministic order.
    let mut seen = HashSet::new();
    let files: Vec<&Path> = candidates
        .iter()
        .filter(|c| seen.insert(c.file_path.as_path()))
        .map(|c| c.file_path.as_path())
        .collect();

    let mut resolutions: Vec<Resolution> = files
        .par_iter()
        .flat_map_iter(|path| matches_in_file(path, target, mode, eng))
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
///
/// When the first parse has errors (commonly an annotation macro between the
/// return type and the function name — dllimport/export style — which
/// tree-sitter has no preprocessor to expand, dropping or mangling overloads),
/// the macros are blanked and the file is re-parsed. If that yields a strictly
/// cleaner parse its matches *replace* the first parse's: the recovered tree is
/// a superset that reports each overload as one clean node, so unioning would
/// double-count an overload the broken parse had already matched (with a
/// different, narrower span). Blanking is length-preserving, so the recovered
/// tree shares the original byte coordinates and we always slice the original
/// `src` for reported text — the macro stays visible in output.
fn matches_in_file(
    path: &Path,
    target: &str,
    mode: Mode,
    eng: &SyntacticEngine,
) -> Vec<Resolution> {
    let src = match std::fs::read(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let tree = match parse(&src) {
        Some(t) => t,
        None => return Vec::new(),
    };

    if tree.root_node().has_error()
        && let Some(neutralized) = neutralize_for(eng, &src)
        && let Some(recovered) = parse(&neutralized)
        && error_count(recovered.root_node()) < error_count(tree.root_node())
    {
        let mut out = Vec::new();
        walk(recovered.root_node(), &src, target, mode, path, &mut out);
        return out;
    }

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
    let (bare, explicit_qualified) = name_text(decl, src)?;
    // Fold in enclosing namespace/class scopes so a namespace-qualified target
    // (`duckdb::UpdateInfo`) matches a bare declaration nested in that scope
    // (design-specs §7.2).
    let qualified = qualify(node, &bare, explicit_qualified, src);

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
        // Class members: a function prototype is a method, otherwise a data
        // member (design-specs §7.2, §11).
        "field_declaration" if mode == Mode::Declaration => {
            Some(if is_function_prototype(node) {
                Kind::Method
            } else {
                Kind::Member
            })
        }
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
    if let Some(decl) = node.child_by_field_name("declarator") {
        // Peel pointer/reference wrappers down to a function_declarator.
        let mut cur = decl;
        loop {
            match cur.kind() {
                "function_declarator" => return true,
                "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                    match inner_declarator(cur) {
                        Some(inner) => cur = inner,
                        None => break,
                    }
                }
                _ => break,
            }
        }
    }
    // Fallback: scan direct children for a function_declarator.
    // tree-sitter-cpp may not place it under the `declarator` field when an
    // unknown macro annotation (e.g. `LIB_API`) sits between the
    // return type and the function name.
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == "function_declarator")
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
            if let Some(decl) = node.child_by_field_name("declarator")
                && let Some(name) = innermost_name(decl)
            {
                return Some(name);
            }
            // Fallback: scan direct children for a function_declarator whose
            // inner name we can extract. Handles declarations with a macro
            // annotation between the return type and function name, e.g.:
            //   static CWideStr LIB_API Encode(...)
            // where tree-sitter may not route the function_declarator through
            // the `declarator` field.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "function_declarator"
                    && let Some(name) = innermost_name(child)
                {
                    return Some(name);
                }
            }
            None
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
            let inner = inner_declarator(node)?;
            innermost_name(inner)
        }
        _ => inner_declarator(node).and_then(innermost_name),
    }
}

/// The inner declarator of a wrapper node.
///
/// tree-sitter-cpp labels the child with a `declarator` field for most wrappers
/// (`pointer_declarator`, `function_declarator`, …) but **not** for
/// `reference_declarator`, where it is an unnamed child after the `&`/`&&`
/// token. This peels both uniformly so reference returns/variables resolve.
fn inner_declarator(node: Node) -> Option<Node> {
    if let Some(d) = node.child_by_field_name("declarator") {
        return Some(d);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|c| {
        matches!(
            c.kind(),
            "identifier"
                | "field_identifier"
                | "type_identifier"
                | "qualified_identifier"
                | "destructor_name"
                | "operator_name"
                | "function_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "array_declarator"
                | "parenthesized_declarator"
                | "init_declarator"
        )
    })
}

/// Build the fully-qualified name from a node's explicit qualifier (e.g. an
/// out-of-line `Foo::bar`) and any enclosing namespace/class scopes. Returns
/// `None` only for a bare name at translation-unit scope.
fn qualify(node: Node, bare: &str, explicit: Option<String>, src: &[u8]) -> Option<String> {
    match (explicit, enclosing_qualifier(node, src)) {
        (Some(e), Some(ns)) => Some(format!("{ns}::{e}")),
        (Some(e), None) => Some(e),
        (None, Some(ns)) => Some(format!("{ns}::{bare}")),
        (None, None) => None,
    }
}

/// Concatenate the names of `namespace`/`class`/`struct` ancestors of `node`,
/// outermost first (e.g. `duckdb::Catalog`). `None` at translation-unit scope.
/// Anonymous namespaces (no `name` field) contribute nothing.
fn enclosing_qualifier(node: Node, src: &[u8]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut cur = node.parent();
    while let Some(n) = cur {
        if matches!(
            n.kind(),
            "namespace_definition" | "class_specifier" | "struct_specifier"
        ) && let Some(name) = n.child_by_field_name("name")
        {
            parts.push(text(name, src));
        }
        cur = n.parent();
    }
    if parts.is_empty() {
        None
    } else {
        parts.reverse();
        Some(parts.join("::"))
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
    let decl = node.child_by_field_name("declarator")?;
    if let Some(func) = find_function_declarator(decl) {
        // Prefer a trailing return type (`auto f() -> T`); otherwise the base
        // type with leading qualifiers and any pointer/reference markers.
        let ret = trailing_return(func, src)
            .or_else(|| rendered_type(node, src))
            .unwrap_or_default();
        let params = func.child_by_field_name("parameters")?;
        let mut cursor = params.walk();
        let types: Vec<String> = params
            .children(&mut cursor)
            .filter(|c| c.kind() == "parameter_declaration")
            .filter_map(|p| rendered_type(p, src))
            .collect();
        Some(format!("{}({})", ret.trim(), types.join(", ")))
    } else {
        rendered_type(node, src)
    }
}

/// Render the type of a `declaration`/`parameter_declaration`/`field_declaration`
/// as `[qualifiers] base [*&]`, e.g. `const Widget &`. Best-effort: captures
/// leading `const`/`volatile` and pointer/reference markers from the declarator.
fn rendered_type(node: Node, src: &[u8]) -> Option<String> {
    let base = node.child_by_field_name("type").map(|t| text(t, src))?;
    let mut prefix = String::new();
    let mut cursor = node.walk();
    for ch in node.children(&mut cursor) {
        if ch.kind() == "type_qualifier" {
            if !prefix.is_empty() {
                prefix.push(' ');
            }
            prefix.push_str(text(ch, src).trim());
        }
    }
    let suffix = node
        .child_by_field_name("declarator")
        .map(|d| pointer_markers(d, src))
        .unwrap_or_default();
    let mut out = String::new();
    if !prefix.is_empty() {
        out.push_str(&prefix);
        out.push(' ');
    }
    out.push_str(base.trim());
    if !suffix.is_empty() {
        out.push(' ');
        out.push_str(&suffix);
    }
    Some(out)
}

/// Collect pointer/reference markers (`*`, `&`, `&&`) wrapping a declarator,
/// stopping at the function declarator or terminal name.
fn pointer_markers(node: Node, src: &[u8]) -> String {
    let mut markers = String::new();
    let mut cur = node;
    loop {
        match cur.kind() {
            "pointer_declarator" => markers.push('*'),
            "reference_declarator" => {
                // The leading token is `&` or `&&`.
                match cur.child(0) {
                    Some(tok) => markers.push_str(&text(tok, src)),
                    None => markers.push('&'),
                }
            }
            _ => break,
        }
        match inner_declarator(cur) {
            Some(inner) => cur = inner,
            None => break,
        }
    }
    markers
}

/// A function declarator's trailing return type (`-> T`), if present.
fn trailing_return(func: Node, src: &[u8]) -> Option<String> {
    let mut cursor = func.walk();
    for ch in func.children(&mut cursor) {
        if ch.kind() == "trailing_return_type" {
            return Some(text(ch, src).trim_start_matches("->").trim().to_string());
        }
    }
    None
}

/// Find a `function_declarator` by peeling pointer/reference wrappers.
fn find_function_declarator(node: Node) -> Option<Node> {
    let mut cur = node;
    loop {
        match cur.kind() {
            "function_declarator" => return Some(cur),
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator"
            | "init_declarator" => {
                cur = inner_declarator(cur)?;
            }
            _ => return None,
        }
    }
}

/// Collect the contiguous comment block immediately above `node` (design-specs
/// §7.2: line `//` runs or `/* ... */` blocks). Returns `None` if absent.
///
/// When a declaration uses a macro annotation between the return type and the
/// function name (e.g. `static CWideStr LIB_API Func(...)`),
/// tree-sitter-cpp splits it into two sibling nodes at the same source line:
/// a `field_declaration` for the type/macro prefix and a `declaration` for the
/// function. The engine matches on the `declaration` node; to find the doc
/// comment we must step over the same-row `field_declaration` fragment that
/// sits between the comment and our node.
fn leading_doc(node: Node, src: &[u8]) -> Option<String> {
    let node_start_row = node.start_position().row;

    // Strategy 1: prev_sibling() chain, stepping over any same-row
    // field_declaration that is the macro-prefix fragment of our declaration.
    {
        let mut comments = Vec::new();
        let mut anchor_row = node_start_row;
        let mut prev = node.prev_sibling();
        while let Some(p) = prev {
            match p.kind() {
                "comment" => {
                    if p.end_position().row + 1 < anchor_row { break; }
                    comments.push(text(p, src));
                    anchor_row = p.start_position().row;
                    prev = p.prev_sibling();
                }
                "field_declaration" if p.start_position().row == node_start_row => {
                    // Same-line fragment (type/macro prefix) — step over it.
                    prev = p.prev_sibling();
                }
                _ => break,
            }
        }
        if !comments.is_empty() {
            comments.reverse();
            return Some(comments.join("\n"));
        }
    }

    // Strategy 2: parent-children scan — same step-over logic applied to the
    // ordered siblings list. Serves as a fallback when prev_sibling() skips
    // extra nodes on certain tree-sitter versions.
    if let Some(parent) = node.parent() {
        let mut cursor = parent.walk();
        let siblings: Vec<_> = parent.children(&mut cursor).collect();
        if let Some(idx) = siblings.iter().position(|s| s.id() == node.id()) {
            let mut comments = Vec::new();
            let mut anchor_row = node_start_row;
            let mut i = idx;
            while i > 0 {
                i -= 1;
                let s = siblings[i];
                match s.kind() {
                    "comment" => {
                        if s.end_position().row + 1 < anchor_row { break; }
                        comments.push(text(s, src));
                        anchor_row = s.start_position().row;
                    }
                    "field_declaration" if s.start_position().row == node_start_row => {
                        // Step over same-line type/macro prefix fragment.
                    }
                    _ => break,
                }
            }
            if !comments.is_empty() {
                comments.reverse();
                return Some(comments.join("\n"));
            }
        }
    }

    None
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

    #[test]
    fn method_prototype_kind_and_qualified_name() {
        let dir = TempDir::new().unwrap();
        let body = "namespace ns {\nclass C {\n    int compute(double x);\n};\n}\n";
        let p = write(&dir, "c.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("compute", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].symbol.kind, Kind::Method);
        assert_eq!(res[0].symbol.qualified_name.as_deref(), Some("ns::C::compute"));
        assert_eq!(res[0].symbol.type_spelling.as_deref(), Some("int(double)"));
    }

    #[test]
    fn namespaced_type_matches_qualified_target() {
        let dir = TempDir::new().unwrap();
        let body = "namespace duckdb {\nstruct UpdateInfo { int x; };\n}\n";
        let p = write(&dir, "u.hpp", body);
        let eng = SyntacticEngine::new();
        assert_eq!(eng.definitions("duckdb::UpdateInfo", &candidates_for(&p)).len(), 1);
        assert_eq!(eng.definitions("UpdateInfo", &candidates_for(&p)).len(), 1);
        assert_eq!(eng.definitions("wrong::UpdateInfo", &candidates_for(&p)).len(), 0);
    }

    #[test]
    fn block_comment_doc_is_captured() {
        let dir = TempDir::new().unwrap();
        let body = "/** Frobnicate the widget.\n *  @param n count\n */\nvoid Frob(int n);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Frob", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let doc = res[0].symbol.doc.as_deref().unwrap();
        assert!(doc.starts_with("/**"));
        assert!(doc.contains("@param n count"));
    }

    #[test]
    fn declaration_return_type_includes_const_ref() {
        let dir = TempDir::new().unwrap();
        // Primitive base type so tree-sitter parses the reference return
        // unambiguously (an undeclared user type + `&` is a Stage-1 limit).
        let body = "const int &Clamp(int x);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Clamp", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        assert_eq!(
            res[0].symbol.type_spelling.as_deref(),
            Some("const int &(int)")
        );
    }

    #[test]
    fn declaration_pointer_return_type() {
        let dir = TempDir::new().unwrap();
        let body = "int *Allocate(size_t n);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Allocate", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].symbol.type_spelling.as_deref(), Some("int *(size_t)"));
    }

    #[test]
    fn declaration_trailing_return_type() {
        let dir = TempDir::new().unwrap();
        let body = "auto Compute(int x) -> double;\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Compute", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].symbol.type_spelling.as_deref(), Some("double(int)"));
    }

    // --- macro-annotation overload tests (e.g. LIB_API-style) ---

    #[test]
    fn declarations_finds_single_line_macro_annotated_overloads() {
        // Reproduces the pattern: `static RetType MACRO FuncName(params);`
        // where MACRO is an unknown annotation that may sit between the return
        // type and the function_declarator in tree-sitter's parse tree.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "#define MOCK_EXPORT __declspec(dllexport)\n",
            "class Coder {\n",
            "public:\n",
            "    static int MOCK_EXPORT Convert(const char* a, bool flag = false);\n",
            "    static int MOCK_EXPORT Convert(const char* a, int n, bool flag = false);\n",
            "};\n",
        );
        let p = write(&dir, "coder.h", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Convert", &candidates_for(&p));
        assert_eq!(res.len(), 2, "both macro-annotated overloads should be found");
        let names: Vec<_> = res.iter().map(|r| r.symbol.name.as_str()).collect();
        assert!(names.iter().all(|&n| n == "Convert"));
    }

    #[test]
    fn declarations_finds_multiline_macro_annotated_overloads() {
        // Two overloads in the same class, both with a macro annotation,
        // multi-line parameter lists, and default values on split lines.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "#define MOCK_EXPORT __declspec(dllexport)\n",
            "class Coder {\n",
            "public:\n",
            "    static int MOCK_EXPORT Convert(\n",
            "        const char* session,\n",
            "        const char* input,\n",
            "        bool useEntities,\n",
            "        int lang = 0,\n",
            "        int mode =\n",
            "            DefaultMode,\n",
            "        bool urduMode = false);\n",
            "\n",
            "    static int MOCK_EXPORT Convert(\n",
            "        const char* input,\n",
            "        bool useEntities,\n",
            "        int lang = 0,\n",
            "        int mode =\n",
            "            DefaultMode,\n",
            "        bool urduMode = false);\n",
            "};\n",
        );
        let p = write(&dir, "coder.h", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Convert", &candidates_for(&p));
        assert_eq!(res.len(), 2, "both multiline macro-annotated overloads should be found");
    }

    #[test]
    fn declarations_finds_macro_annotated_overload_with_doxygen_comments() {
        // Full pattern: Doxygen block comment above each overload, macro
        // annotation, multi-line params.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "#define MOCK_EXPORT __declspec(dllexport)\n",
            "class Coder {\n",
            "public:\n",
            "    /**\n",
            "     * Overload 1 — with session.\n",
            "     * @param session The session.\n",
            "     * @param input The input.\n",
            "     */\n",
            "    static int MOCK_EXPORT Convert(\n",
            "        const char* session,\n",
            "        const char* input);\n",
            "\n",
            "    /**\n",
            "     * Overload 2 — default session.\n",
            "     * @param input The input.\n",
            "     */\n",
            "    static int MOCK_EXPORT Convert(\n",
            "        const char* input);\n",
            "};\n",
        );
        let p = write(&dir, "coder.h", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Convert", &candidates_for(&p));
        assert_eq!(res.len(), 2, "both overloads with Doxygen comments should be found");
        // Each result should carry its own doc comment.
        for r in &res {
            assert!(
                r.symbol.doc.as_deref().unwrap_or("").contains("Overload"),
                "expected doc comment, got {:?}", r.symbol.doc
            );
        }
    }

    #[test]
    fn declarations_finds_pure_virtual_macro_annotated_overloads() {
        // The key breaker: a pure-virtual interface with a dllimport/export macro
        // between the return type and the name. tree-sitter reparses one `= 0`
        // overload as a function_definition (skipped by find-decl) and wraps the
        // macro of the other in an ERROR node — dropping an overload.
        // Macro neutralization (via per-file #define harvest) recovers both.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "#define LIB_API __declspec(dllimport)\n",
            "class ICodec {\n",
            "public:\n",
            "    virtual CWideStr LIB_API Convert(const CStr& a) = 0;\n",
            "    virtual CWideStr LIB_API Convert(const CStr& a, int n) = 0;\n",
            "};\n",
        );
        let p = write(&dir, "icodec.h", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Convert", &candidates_for(&p));
        assert_eq!(res.len(), 2, "both pure-virtual macro overloads should be found");
        // Byte-fidelity holds: each reported span re-slices from disk exactly,
        // and the macro text remains visible (we slice the original, not the
        // blanked buffer).
        let disk = fs::read(&p).unwrap();
        for r in &res {
            let s = &r.source_ref.span;
            assert_eq!(&disk[s.start_byte..s.end_byte], r.content_bytes.as_slice());
            assert!(String::from_utf8_lossy(&r.content_bytes).contains("LIB_API"));
        }
    }

    #[test]
    fn empty_macro_config_recovers_mixed_case_annotation() {
        // A mixed-case annotation macro the UPPER_CASE auto-detector won't catch;
        // the user supplies it via `--empty-macro` (with_empty_macros). This
        // form breaks the parse hard — plain resolution finds neither overload.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "class ICodec {\n",
            "public:\n",
            "    virtual CWideStr Codec_Export Convert(const CStr& a) = 0;\n",
            "    virtual CWideStr Codec_Export Convert(const CStr& a, int n) = 0;\n",
            "};\n",
        );
        let p = write(&dir, "icodec.h", body);
        // Without config, auto-detect (UPPER_CASE only) cannot recover it.
        let plain = SyntacticEngine::new();
        assert_eq!(plain.declarations("Convert", &candidates_for(&p)).len(), 0);
        // With config, both overloads are recovered.
        let eng = SyntacticEngine::with_empty_macros(["Codec_Export".to_string()]);
        assert_eq!(eng.declarations("Convert", &candidates_for(&p)).len(), 2);
    }
}
