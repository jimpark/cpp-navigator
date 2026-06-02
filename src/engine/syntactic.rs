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
        let tree = best_tree(&src, self)?;
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
        let tree = best_tree(&src, self)?;
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

    fn filter_references(&self, target: &str, candidates: &[Candidate]) -> Vec<Candidate> {
        // Only a qualified target carries the scope needed to rule anything out.
        if !target.contains("::") {
            return candidates.to_vec();
        }
        let bare = target.rsplit("::").next().unwrap_or(target);
        // Candidates arrive grouped by file (finder order). Parse each file once,
        // caching the most recent (path, src, tree) across consecutive hits.
        let mut out = Vec::with_capacity(candidates.len());
        let mut cache: Option<(std::path::PathBuf, Vec<u8>, Tree)> = None;
        for c in candidates {
            let stale = cache.as_ref().map(|(p, _, _)| p != &c.file_path).unwrap_or(true);
            if stale {
                cache = std::fs::read(&c.file_path)
                    .ok()
                    .and_then(|src| best_tree(&src, self).map(|tree| (c.file_path.clone(), src, tree)));
            }
            // Unreadable/unparsable file: keep the hit (never drop on uncertainty).
            let keep = match &cache {
                Some((_, src, tree)) => {
                    keep_reference_line(tree.root_node(), src, c.byte_offset, bare, target)
                }
                None => true,
            };
            if keep {
                out.push(c.clone());
            }
        }
        out
    }
}

/// Whether a find-refs hit *line* survives the qualified-target precision pass.
///
/// `line_start` is the byte offset of the line within `src` (a finder
/// `Candidate` records the line start, not the token). We inspect every
/// determinable occurrence of `bare` on that line and drop the line only when
/// *all* of them resolve to a scope incompatible with `target`. A line with no
/// determinable occurrence (token only in a comment/string, or a scope the
/// parser cannot place) is kept.
fn keep_reference_line(root: Node, src: &[u8], line_start: usize, bare: &str, target: &str) -> bool {
    let line_end = src[line_start.min(src.len())..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|i| line_start + i)
        .unwrap_or(src.len());
    let mut total = 0usize;
    let mut excluded = 0usize;
    count_ref_verdicts(root, src, line_start, line_end, bare, target, &mut total, &mut excluded);
    total == 0 || excluded < total
}

/// Walk the subtree, tallying name-final occurrences of `bare` on the line and
/// how many resolve to a scope incompatible with `target`.
#[allow(clippy::too_many_arguments)]
fn count_ref_verdicts(
    node: Node,
    src: &[u8],
    line_start: usize,
    line_end: usize,
    bare: &str,
    target: &str,
    total: &mut usize,
    excluded: &mut usize,
) {
    // A node fully outside the line range contains no in-range descendants.
    if node.end_byte() <= line_start || node.start_byte() >= line_end {
        return;
    }
    if matches!(node.kind(), "identifier" | "field_identifier")
        && node.start_byte() >= line_start
        && node.start_byte() < line_end
        && is_name_final(node)
        && text(node, src) == bare
    {
        *total += 1;
        if reference_excluded(node, src, bare, target) {
            *excluded += 1;
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_ref_verdicts(child, src, line_start, line_end, bare, target, total, excluded);
    }
}

/// Is `node` the final name of its (possibly qualified) reference — i.e. not a
/// left-hand scope component of a `qualified_identifier` like the `A` in `A::b`?
fn is_name_final(node: Node) -> bool {
    match node.parent() {
        Some(p) if p.kind() == "qualified_identifier" => {
            p.child_by_field_name("name").map(|n| n.id()) == Some(node.id())
        }
        _ => true,
    }
}

/// Can the reference at `node` be proven to denote a *different* entity than the
/// qualified `target`? True only when the reference carries a determinable scope
/// (an explicit `Other::name` qualifier, or a member declared in a class) that
/// is incompatible with `target`. Bare calls and member accesses are unknowable
/// here and return false (keep).
fn reference_excluded(node: Node, src: &[u8], bare: &str, target: &str) -> bool {
    match reference_qualified_name(node, src, bare) {
        Some(q) => !qualified_compatible(target, &q),
        None => false,
    }
}

/// The scope-qualified name a reference denotes, when determinable:
///  * explicit `A::B::name` use or out-of-line definition → `"A::B::name"`;
///  * a member declared in a class body (`field_identifier` in declarator
///    position) → the enclosing `ns::Class::name`.
///
/// Member accesses (`x.name`/`p->name`) and bare uses return `None` (unknown).
fn reference_qualified_name(node: Node, src: &[u8], bare: &str) -> Option<String> {
    let parent = node.parent()?;
    match parent.kind() {
        "qualified_identifier" => {
            // Climb to the outermost qualifier so the whole `A::B::name` is read.
            let mut top = parent;
            while let Some(gp) = top.parent() {
                if gp.kind() == "qualified_identifier" {
                    top = gp;
                } else {
                    break;
                }
            }
            Some(text(top, src))
        }
        // `x.name` / `p->name`: the receiver's type is not syntactically known.
        "field_expression" => None,
        // A bare `field_identifier` in declarator position names a member; its
        // owner is the enclosing class/namespace scope.
        _ if node.kind() == "field_identifier" => qualify(node, bare, None, src),
        _ => None,
    }
}

/// Are two names ending in the same bare component scope-compatible — is one's
/// qualifier a component-aligned suffix of the other's? `CTextBuffer::m` is
/// compatible with `ns::CTextBuffer::m` (partial qualification at a use site)
/// but not with `Other::m`.
fn qualified_compatible(target: &str, q: &str) -> bool {
    let bare = target.rsplit("::").next().unwrap_or(target);
    name_matches(target, bare, Some(q)) || name_matches(q, bare, Some(target))
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
/// `#define`s into the annotation set first and adding `extra_global` (e.g.
/// class-annotation macros discovered structurally) to the blank-everywhere set.
/// Length-preserving; `None` if nothing was blanked.
fn neutralize_for(eng: &SyntacticEngine, src: &[u8], extra_global: &HashSet<String>) -> Option<Vec<u8>> {
    let mut annotation = eng.annotation_macros.clone();
    macros::collect_defines(src, &mut annotation);
    let global = if extra_global.is_empty() {
        eng.global_macros.clone()
    } else {
        eng.global_macros.union(extra_global).cloned().collect()
    };
    macros::neutralize(src, &global, &annotation)
}

/// Parse `src`, recovering from annotation-macro damage when present.
///
/// Two kinds of damage are repaired by blanking macros and reparsing:
///  * **Error-driven** — the first parse has `ERROR`/`MISSING` nodes (commonly a
///    function annotation between return type and name). Recovery is accepted
///    only if it yields a strictly cleaner parse.
///  * **Silent class misparse** — `class MACRO Name { … }` parses *without error*
///    as a function returning a bodyless `class MACRO` (see
///    [`discover_class_annotation_macros`]). Recovery is accepted as long as it
///    does not add errors, since the original parse hid the class scope.
///
/// Blanking is length-preserving (see [`macros`]), so the returned tree's byte
/// offsets still index the *original* `src` — callers must continue slicing
/// `src`, never the blanked buffer.
fn best_tree(src: &[u8], eng: &SyntacticEngine) -> Option<Tree> {
    let tree = parse(src)?;
    let class_macros = discover_class_annotation_macros(tree.root_node(), src);
    if !tree.root_node().has_error() && class_macros.is_empty() {
        return Some(tree);
    }
    if let Some(neutralized) = neutralize_for(eng, src, &class_macros)
        && let Some(recovered) = parse(&neutralized)
    {
        let before = error_count(tree.root_node());
        let after = error_count(recovered.root_node());
        // A discovered class misparse is error-free, so require only that the
        // reparse not regress; the pure error-recovery path still demands a
        // strict improvement.
        let accept = if class_macros.is_empty() {
            after < before
        } else {
            after <= before
        };
        if accept {
            return Some(recovered);
        }
    }
    Some(tree)
}

/// Names of class/struct annotation macros that tree-sitter silently misparses.
///
/// A dllexport-style annotation between `class` and the class name —
/// `class TEXTUTIL_DSPEC CTextBuffer { … }` — parses *without error* as a
/// function named `CTextBuffer` returning a bodyless `class TEXTUTIL_DSPEC`, with
/// the class body captured as a `compound_statement`. The class scope then
/// disappears, so members resolve with the wrong qualified name (the enclosing
/// class is missing). The shape is unambiguous: a plain-identifier declarator can
/// never carry a function body in valid C++, so a `function_definition` of that
/// shape is always this misparse. We return the mis-slotted token(s) so they can
/// be blanked and the file reparsed as a real class.
fn discover_class_annotation_macros(root: Node, src: &[u8]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_class_annotation_macros(root, src, &mut out);
    out
}

fn collect_class_annotation_macros(node: Node, src: &[u8], out: &mut HashSet<String>) {
    if node.kind() == "function_definition"
        && let Some(ty) = node.child_by_field_name("type")
        && matches!(ty.kind(), "class_specifier" | "struct_specifier" | "union_specifier")
        && ty.child_by_field_name("body").is_none()
        && let Some(name) = ty.child_by_field_name("name")
        && node.child_by_field_name("declarator").map(|d| d.kind()) == Some("identifier")
        && node.child_by_field_name("body").map(|b| b.kind()) == Some("compound_statement")
    {
        out.insert(text(name, src));
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_class_annotation_macros(child, src, out);
    }
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
/// The file is parsed via [`best_tree`], which transparently blanks annotation
/// macros and reparses when the first parse is damaged — either by an `ERROR`
/// node (a function annotation between the return type and name, dropping or
/// mangling overloads) or by the silent `class MACRO Name { … }` misparse that
/// hides the class scope without any error. Blanking is length-preserving, so
/// the tree shares the original byte coordinates and we always slice the
/// original `src` for reported text — the macro stays visible in output.
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
    let tree = match best_tree(&src, eng) {
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
    let decl_span = span_of(report);

    // Extend the reported span backward to include any leading doc comment so
    // that `content` carries the full context (comment + declaration), matching
    // the behaviour of the text-fallback path. The `doc` field is also populated
    // for structured access. Applied to both definitions and declarations.
    let doc_info = leading_doc(report, src);
    let span = match &doc_info {
        Some(d) => Span {
            start_byte: d.start_byte,
            start_line: d.start_line,
            start_col: d.start_col,
            ..decl_span
        },
        None => decl_span,
    };
    let content_bytes = src[span.start_byte..span.end_byte].to_vec();

    let (signature, type_spelling, doc) = if mode == Mode::Declaration {
        (
            Some(text(node, src).trim().to_string()),
            type_spelling(node, src),
            doc_info.map(|d| d.text),
        )
    } else {
        (None, None, doc_info.map(|d| d.text))
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
/// qualified name to end with the target on a `::` component boundary.
///
/// The suffix must be component-aligned: `Calc::square` matches `ns::Calc::square`
/// but **not** `ns::MiniCalc::square` (where `Calc::square` is only a mid-token
/// substring suffix). This matters most for `find-decl`, where the qualifier is
/// reconstructed from enclosing namespace/class scopes (see [`qualify`]) rather
/// than written explicitly as in an out-of-line definition.
fn name_matches(target: &str, bare: &str, qualified: Option<&str>) -> bool {
    let target_bare = target.rsplit("::").next().unwrap_or(target);
    if bare != target_bare {
        return false;
    }
    if target.contains("::") {
        match qualified {
            // Exact, or a component-aligned suffix (the `::` before `target`
            // guarantees alignment, so `Calc::x` never matches `MiniCalc::x`).
            Some(q) => q == target || q.ends_with(&format!("::{target}")),
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

/// The leading doc comment block immediately above a node, together with the
/// byte position where that block starts. Used to extend content spans so the
/// comment is visible in the `content` payload, not only in the `doc` field.
struct LeadingDoc {
    text: String,
    /// Byte offset of the first character of the topmost comment line.
    start_byte: usize,
    /// 1-based line of `start_byte`.
    start_line: usize,
    /// 0-based byte column of `start_byte` within its line.
    start_col: usize,
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
fn leading_doc(node: Node, src: &[u8]) -> Option<LeadingDoc> {
    let node_start_row = node.start_position().row;

    // Strategy 1: prev_sibling() chain, stepping over any same-row
    // field_declaration that is the macro-prefix fragment of our declaration.
    {
        let mut comments: Vec<Node> = Vec::new();
        let mut anchor_row = node_start_row;
        let mut prev = node.prev_sibling();
        while let Some(p) = prev {
            match p.kind() {
                "comment" => {
                    if p.end_position().row + 1 < anchor_row { break; }
                    comments.push(p);
                    anchor_row = p.start_position().row;
                    prev = p.prev_sibling();
                }
                "field_declaration" if p.start_position().row == node_start_row => {
                    prev = p.prev_sibling();
                }
                _ => break,
            }
        }
        if !comments.is_empty() {
            comments.reverse(); // now oldest-first
            let top = comments[0];
            return Some(LeadingDoc {
                text: comments.iter().map(|n| text(*n, src)).collect::<Vec<_>>().join("\n"),
                start_byte: top.start_byte(),
                start_line: top.start_position().row + 1,
                start_col: top.start_position().column,
            });
        }
    }

    // Strategy 2: parent-children scan — same step-over logic applied to the
    // ordered siblings list. Serves as a fallback when prev_sibling() skips
    // extra nodes on certain tree-sitter versions.
    if let Some(parent) = node.parent() {
        let mut cursor = parent.walk();
        let siblings: Vec<_> = parent.children(&mut cursor).collect();
        if let Some(idx) = siblings.iter().position(|s| s.id() == node.id()) {
            let mut comments: Vec<Node> = Vec::new();
            let mut anchor_row = node_start_row;
            let mut i = idx;
            while i > 0 {
                i -= 1;
                let s = siblings[i];
                match s.kind() {
                    "comment" => {
                        if s.end_position().row + 1 < anchor_row { break; }
                        comments.push(s);
                        anchor_row = s.start_position().row;
                    }
                    "field_declaration" if s.start_position().row == node_start_row => {}
                    _ => break,
                }
            }
            if !comments.is_empty() {
                comments.reverse();
                let top = comments[0];
                return Some(LeadingDoc {
                    text: comments.iter().map(|n| text(*n, src)).collect::<Vec<_>>().join("\n"),
                    start_byte: top.start_byte(),
                    start_line: top.start_position().row + 1,
                    start_col: top.start_position().column,
                });
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
    fn declaration_content_includes_leading_comment() {
        // The `content` payload must start at the doc comment, not the
        // declaration keyword, so LLMs get the full context in one field.
        let dir = TempDir::new().unwrap();
        let body = "/// Allocate the global pool.\nvoid InitPool(size_t n);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("InitPool", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let content = String::from_utf8_lossy(&res[0].content_bytes);
        assert!(content.starts_with("/// Allocate"), "content should start with the doc comment");
        assert!(content.contains("void InitPool"), "content should include the declaration");
        // Byte-fidelity: re-slice from disk equals reported content.
        let disk = fs::read(&p).unwrap();
        let span = &res[0].source_ref.span;
        assert_eq!(&disk[span.start_byte..span.end_byte], res[0].content_bytes.as_slice());
        assert_eq!(span.start_line, 1, "span should start at the comment line");
    }

    #[test]
    fn definition_content_includes_leading_comment() {
        // Definitions also extend their span to cover the leading comment.
        let dir = TempDir::new().unwrap();
        let body = "/// Compute the sum.\nint add(int a, int b) {\n    return a + b;\n}\n";
        let p = write(&dir, "a.cpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.definitions("add", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let content = String::from_utf8_lossy(&res[0].content_bytes);
        assert!(content.starts_with("/// Compute"), "definition content should start with comment");
        assert!(content.contains("return a + b"), "definition content should include body");
        // Byte-fidelity.
        let disk = fs::read(&p).unwrap();
        let span = &res[0].source_ref.span;
        assert_eq!(&disk[span.start_byte..span.end_byte], res[0].content_bytes.as_slice());
        assert_eq!(span.start_line, 1);
    }

    #[test]
    fn no_comment_span_is_unchanged() {
        // When there is no leading comment the span and content are as before.
        let dir = TempDir::new().unwrap();
        let body = "void InitPool(size_t n);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("InitPool", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let content = String::from_utf8_lossy(&res[0].content_bytes);
        assert!(content.starts_with("void InitPool"), "no comment — content starts at declaration");
        assert_eq!(res[0].source_ref.span.start_line, 1);
        assert!(res[0].symbol.doc.is_none());
    }

    #[test]
    fn block_comment_included_in_content_with_byte_fidelity() {
        // Multi-line Doxygen block comment is included and byte-exact.
        let dir = TempDir::new().unwrap();
        let body = "/**\n * @brief Frobnicate.\n * @param n count\n */\nvoid Frob(int n);\n";
        let p = write(&dir, "a.hpp", body);
        let eng = SyntacticEngine::new();
        let res = eng.declarations("Frob", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        let content = String::from_utf8_lossy(&res[0].content_bytes);
        assert!(content.starts_with("/**"), "block comment must be in content");
        assert!(content.contains("void Frob(int n);"), "declaration must follow comment in content");
        let disk = fs::read(&p).unwrap();
        let span = &res[0].source_ref.span;
        assert_eq!(&disk[span.start_byte..span.end_byte], res[0].content_bytes.as_slice());
        assert_eq!(span.start_line, 1);
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
    fn qualified_target_requires_component_aligned_suffix() {
        // `Calc::square` must NOT match `MiniCalc::square`: the `Calc` token in
        // the target has to align with a whole `::` component of the node's
        // qualified name, not merely be a substring suffix of one.
        let dir = TempDir::new().unwrap();
        let body = "namespace ns {\nclass MiniCalc {\n    int square(int x);\n};\n}\n";
        let p = write(&dir, "m.hpp", body);
        let eng = SyntacticEngine::new();
        assert_eq!(eng.declarations("Calc::square", &candidates_for(&p)).len(), 0);
        // The genuine class still resolves, qualified or bare.
        assert_eq!(eng.declarations("MiniCalc::square", &candidates_for(&p)).len(), 1);
        assert_eq!(eng.declarations("ns::MiniCalc::square", &candidates_for(&p)).len(), 1);
        assert_eq!(eng.declarations("square", &candidates_for(&p)).len(), 1);
    }

    #[test]
    fn in_class_method_resolves_by_qualified_name() {
        // find-decl reconstructs the qualifier from the enclosing namespace +
        // class scopes, so a bare in-class prototype is reachable via any
        // component-aligned suffix of `ns::Calc::square`.
        let dir = TempDir::new().unwrap();
        let body = "namespace ns {\nclass Calc {\n    int square(int x);\n};\n}\n";
        let p = write(&dir, "c.hpp", body);
        let eng = SyntacticEngine::new();
        for t in ["square", "Calc::square", "ns::Calc::square"] {
            assert_eq!(
                eng.declarations(t, &candidates_for(&p)).len(),
                1,
                "target {t:?} should resolve the in-class method"
            );
        }
        // A wrong qualifier must not match.
        assert_eq!(eng.declarations("Other::square", &candidates_for(&p)).len(), 0);
    }

    #[test]
    fn class_annotation_macro_does_not_hide_class_scope() {
        // `class MACRO Name { ... }` silently misparses (no error) as a function
        // returning a bodyless `class MACRO`, dropping the class scope. The
        // engine must detect the shape, blank the annotation, and reparse so the
        // member's qualified name keeps the class — `find-decl Name::method`
        // resolves without any --empty-macro flag.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "namespace textutil {\n",
            "class TEXTUTIL_DSPEC CTextBuffer {\n",
            "public:\n",
            "    void assign_from_fn(int fn);\n",
            "};\n",
            "}\n",
        );
        let p = write(&dir, "tb.h", body);
        let eng = SyntacticEngine::new();
        for t in [
            "assign_from_fn",
            "CTextBuffer::assign_from_fn",
            "textutil::CTextBuffer::assign_from_fn",
        ] {
            let res = eng.declarations(t, &candidates_for(&p));
            assert_eq!(res.len(), 1, "target {t:?} should resolve");
            assert_eq!(
                res[0].symbol.qualified_name.as_deref(),
                Some("textutil::CTextBuffer::assign_from_fn"),
                "target {t:?} must keep the class scope"
            );
            assert_eq!(res[0].symbol.kind, Kind::Method);
        }
        // A wrong class qualifier still does not match.
        assert_eq!(
            eng.declarations("Other::assign_from_fn", &candidates_for(&p)).len(),
            0
        );
    }

    #[test]
    fn class_annotation_macro_is_recognized_as_class_definition() {
        // The same misparse otherwise makes `find-def` report the class as a
        // function. After recovery it is a real class definition.
        let dir = TempDir::new().unwrap();
        let body = "struct DLL_EXPORT Widget {\n    int x;\n};\n";
        let p = write(&dir, "w.h", body);
        let eng = SyntacticEngine::new();
        let res = eng.definitions("Widget", &candidates_for(&p));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].symbol.kind, Kind::Struct);
        // Byte-fidelity preserved despite the blank-and-reparse.
        let disk = fs::read(&p).unwrap();
        let s = &res[0].source_ref.span;
        assert_eq!(&disk[s.start_byte..s.end_byte], res[0].content_bytes.as_slice());
        assert!(String::from_utf8_lossy(&res[0].content_bytes).contains("DLL_EXPORT"));
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

    /// Build one line-start candidate per line containing `needle`, mirroring the
    /// finder's whole-line, line-offset hits.
    fn line_candidates(path: &Path, src: &str, needle: &str) -> Vec<Candidate> {
        let mut out = Vec::new();
        let mut offset = 0usize;
        for (i, line) in src.split_inclusive('\n').enumerate() {
            if line.contains(needle) {
                out.push(Candidate {
                    file_path: path.to_path_buf(),
                    line: i + 1,
                    byte_offset: offset,
                    snippet: line.trim_end().to_string(),
                });
            }
            offset += line.len();
        }
        out
    }

    fn kept_lines(eng: &SyntacticEngine, target: &str, cands: &[Candidate]) -> Vec<usize> {
        eng.filter_references(target, cands)
            .iter()
            .map(|c| c.line)
            .collect()
    }

    #[test]
    fn filter_references_drops_other_scopes_keeps_unknown() {
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "namespace app {\n",            // 1
            "class Widget {\n",             // 2
            "    void render();\n",          // 3  Widget member decl  -> keep
            "};\n",                          // 4
            "class Gadget {\n",             // 5
            "    void render();\n",          // 6  Gadget member decl   -> drop
            "};\n",                          // 7
            "void Widget::render() {}\n",   // 8  qualified Widget      -> keep
            "void Gadget::render() {}\n",   // 9  qualified Gadget      -> drop
            "void use(Widget w, Gadget g) {\n", // 10
            "    w.render();\n",             // 11 member access unknown -> keep
            "    g.render();\n",             // 12 member access unknown -> keep
            "    app::Widget::render();\n",  // 13 qualified, compatible -> keep
            "    Gadget::render();\n",       // 14 qualified, incompatible-> drop
            "}\n",                            // 15
            "}\n",                            // 16
        );
        let p = write(&dir, "w.cpp", body);
        let eng = SyntacticEngine::new();
        let cands = line_candidates(&p, body, "render");
        // Bare target: nothing is dropped (no scope to filter on).
        assert_eq!(kept_lines(&eng, "render", &cands).len(), cands.len());
        // Qualified target: only the provably-different occurrences are removed.
        assert_eq!(
            kept_lines(&eng, "Widget::render", &cands),
            vec![3, 8, 11, 12, 13]
        );
        // The fully-qualified form behaves identically.
        assert_eq!(
            kept_lines(&eng, "app::Widget::render", &cands),
            vec![3, 8, 11, 12, 13]
        );
    }

    #[test]
    fn filter_references_keeps_comment_only_hits() {
        // A bare name appearing only inside a comment has no determinable node;
        // erring toward recall, the line is kept.
        let dir = TempDir::new().unwrap();
        let body = concat!(
            "struct A { void run(); };\n",       // 1 A member decl -> drop for B::run
            "struct B { void run(); };\n",       // 2 B member decl -> keep
            "// call run() somewhere\n",          // 3 comment       -> keep
        );
        let p = write(&dir, "c.cpp", body);
        let eng = SyntacticEngine::new();
        let cands = line_candidates(&p, body, "run");
        assert_eq!(kept_lines(&eng, "B::run", &cands), vec![2, 3]);
    }
}
