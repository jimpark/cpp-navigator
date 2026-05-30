//! Engine abstraction (design-specs §5).
//!
//! The key extensibility seam: `SyntacticEngine` (tree-sitter, default,
//! self-contained) and the opt-in `SemanticEngine` (libclang, behind the
//! `semantic` cargo feature) both implement [`Engine`]. The query pipeline is
//! written against the trait, so the libclang backend is a drop-in upgrade.

use std::path::Path;

use crate::model::{Resolution, Span};
use crate::search::Candidate;

mod syntactic;
pub use syntactic::SyntacticEngine;

/// Common interface over the syntactic and semantic backends.
pub trait Engine {
    /// Human/log name, e.g. `"tree-sitter"` or `"libclang"`.
    fn name(&self) -> &str;

    /// Resolve *definition* nodes (with a body/initializer) named `target`
    /// among the given candidate files.
    fn definitions(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution>;

    /// Resolve *declaration* nodes (signature, no body) named `target`.
    fn declarations(&self, target: &str, candidates: &[Candidate]) -> Vec<Resolution>;

    /// Find the enclosing function/template span around a byte offset, for
    /// `--scope` (find-def) and `--context` (find-refs).
    fn enclosing_scope(&self, file: &Path, byte_offset: usize) -> Option<Span>;
}
