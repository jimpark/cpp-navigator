//! Candidate finder (design-specs §5, §10).
//!
//! Stage 0 of the pipeline: narrow a potentially huge tree to the handful of
//! files that *textually mention* the target identifier, honoring ignore rules.
//! Implemented on ripgrep's own library crates (`ignore` + `grep`) so the
//! binary stays self-contained — no external `rg` process.
//!
//! NOTE: implementation lands in Phase 1; this defines the shared type.

use std::path::PathBuf;

/// A textual hit produced by the candidate finder, before any parsing.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub file_path: PathBuf,
    /// 1-based line of the hit.
    pub line: usize,
    /// Byte offset of the hit within the file.
    pub byte_offset: usize,
    /// The raw matched line, for ambiguous-mode snippets.
    pub snippet: String,
}
