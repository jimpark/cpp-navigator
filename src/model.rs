//! Internal data model (design-specs §6).
//!
//! These are the engine-internal types. They are distinct from the
//! serialization types in [`crate::output`], which define the stable wire
//! format. Keeping them separate lets the JSON schema evolve independently of
//! internal refactors.

use std::path::PathBuf;

/// Byte- and line-accurate span within a single file.
///
/// `start_byte..end_byte` is the authoritative slice; line/col are derived
/// conveniences. Byte offsets are what guarantee the fidelity contract
/// (design-specs §8.4): the consumer can re-slice the file and verify a
/// byte-for-byte round-trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start_byte: usize,
    pub end_byte: usize,
    /// 1-based line of `start_byte`.
    pub start_line: usize,
    /// 1-based line of `end_byte`.
    pub end_line: usize,
    /// 0-based byte column within `start_line`.
    pub start_col: usize,
    /// 0-based byte column within `end_line`.
    pub end_col: usize,
}

/// A span anchored to a concrete file.
#[derive(Clone, Debug)]
pub struct SourceRef {
    pub file_path: PathBuf,
    pub span: Span,
}

/// What kind of construct a resolution refers to (design-specs §6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Function,
    Variable,
    Template,
    Class,
    Struct,
    Method,
    Member,
    Macro,
}

/// A resolved C++ symbol and its optional declaration metadata.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub qualified_name: Option<String>,
    pub kind: Kind,
    /// Declaration signature (populated for `find-decl`).
    pub signature: Option<String>,
    /// Type spelling, e.g. `void(size_t)` (populated for `find-decl`).
    pub type_spelling: Option<String>,
    /// Adjacent leading comment / Doxygen block.
    pub doc: Option<String>,
}

/// Outcome class for a query, driving `resolution_type` on the wire
/// (design-specs §6, §9 degradation ladder).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Resolved,
    Ambiguous,
    Fallback,
    NotFound,
}

/// A single resolved result from an [`crate::engine::Engine`].
#[derive(Clone, Debug)]
pub struct Resolution {
    pub symbol: Symbol,
    pub source_ref: SourceRef,
    /// Verbatim bytes `file[start_byte..end_byte)`. Never normalized.
    pub content_bytes: Vec<u8>,
    /// Engine that produced this result, e.g. `"tree-sitter"`.
    pub engine: String,
    /// 0.0..=1.0 confidence; semantic results rank above syntactic.
    pub confidence: f32,
    pub status: Status,
}
