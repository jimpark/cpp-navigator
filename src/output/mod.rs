//! Output serialization (design-specs §8).
//!
//! Defines the stable wire format: a common envelope on every record plus
//! status-specific fields, emitted as JSONL or wrapped in a paste-able bundle.
//! Empty/irrelevant fields are omitted entirely (token economy via omission,
//! never by altering payload bytes — design-specs §8.1, §8.4).

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Format;
use crate::model::Resolution;

pub const SCHEMA_VERSION: &str = "1.0";
pub const TOOL: &str = "cpp-navigator";

/// Wire-level status (snake_case on the wire to match `resolution_type`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Resolved,
    Ambiguous,
    Fallback,
    NotFound,
}

/// One candidate location for ambiguous results (design-specs §8.5).
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub file_path: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// A dense reference location (file + line) for find-refs location-only mode.
#[derive(Clone, Debug, Serialize)]
pub struct RefLocation {
    pub file: String,
    pub line: usize,
}

/// A full resolved result record emitted when showing multiple overloads.
#[derive(Clone, Debug, Serialize)]
pub struct ResolvedResult {
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_spelling: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
}

/// A reference with its enclosing scope context (for --context mode).
#[derive(Clone, Debug, Serialize)]
pub struct RefContext {
    pub file: String,
    pub line: usize,
    pub scope_start_line: usize,
    pub scope_end_line: usize,
    pub content: String,
}

/// A single output record. The envelope fields are always present; everything
/// else is omitted when absent so the consumer can branch on `status` /
/// `resolution_type` without string-scraping.
#[derive(Clone, Debug, Serialize)]
pub struct Record {
    pub schema_version: &'static str,
    pub tool: &'static str,
    pub command: String,
    pub target: String,
    pub status: Status,
    pub resolution_type: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,

    // Resolved definition/declaration -------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_byte: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,

    // find-decl extras ----------------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_spelling: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,

    // Ambiguous -----------------------------------------------------------
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<Candidate>,

    // Multiple resolved results (overloads shown in full) -----------------
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ResolvedResult>,

    // find-refs location-only ---------------------------------------------
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<RefLocation>,

    // find-refs --context -------------------------------------------------
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<RefContext>,

    // Fallback ------------------------------------------------------------
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approximate_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_before: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_after: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_buffer: Option<String>,

    // Cross-cutting markers ----------------------------------------------
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub budget_trimmed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Record {
    /// Base record with only the envelope populated.
    pub fn new(command: &str, target: &str, status: Status, resolution_type: &str) -> Self {
        Record {
            schema_version: SCHEMA_VERSION,
            tool: TOOL,
            command: command.to_string(),
            target: target.to_string(),
            status,
            resolution_type: resolution_type.to_string(),
            engine: None,
            file_path: None,
            start_line: None,
            end_line: None,
            start_byte: None,
            end_byte: None,
            content: None,
            signature: None,
            type_spelling: None,
            doc: None,
            candidates: Vec::new(),
            results: Vec::new(),
            locations: Vec::new(),
            contexts: Vec::new(),
            approximate_line: None,
            window_before: None,
            window_after: None,
            content_buffer: None,
            truncated: false,
            budget_trimmed: false,
            message: None,
        }
    }

    /// The `not_found` rung of the degradation ladder (design-specs §8.7).
    pub fn not_found(command: &str, target: &str) -> Self {
        let mut r = Record::new(command, target, Status::NotFound, "not_found");
        r.message = Some("No textual or semantic match in the searched roots.".to_string());
        r
    }

    /// The `resolved` rung: an engine bounded the target to one exact construct
    /// (design-specs §8.3). `content` is the verbatim byte slice. For `find-decl`
    /// the signature/type/doc fields are populated from the symbol.
    pub fn resolved(command: &str, target: &str, resolution_type: &str, r: &Resolution) -> Self {
        let mut rec = Record::new(command, target, Status::Resolved, resolution_type);
        let span = &r.source_ref.span;
        rec.engine = Some(r.engine.clone());
        rec.file_path = Some(r.source_ref.file_path.to_string_lossy().into_owned());
        rec.start_line = Some(span.start_line);
        rec.end_line = Some(span.end_line);
        rec.start_byte = Some(span.start_byte);
        rec.end_byte = Some(span.end_byte);
        rec.content = Some(String::from_utf8_lossy(&r.content_bytes).into_owned());
        rec.signature = r.symbol.signature.clone();
        rec.type_spelling = r.symbol.type_spelling.clone();
        rec.doc = r.symbol.doc.clone();
        rec
    }

    /// The `ambiguous` rung: an engine found multiple matches (overloads /
    /// redefinitions) and cannot pick one syntactically (design-specs §8.5).
    pub fn ambiguous(command: &str, target: &str, candidates: Vec<Candidate>) -> Self {
        let mut rec = Record::new(command, target, Status::Ambiguous, "ambiguous_multiple_matches");
        rec.message = Some(format!(
            "Found {} candidates. Returning raw candidate locations.",
            candidates.len()
        ));
        rec.candidates = candidates;
        rec
    }

    /// The text-fallback rung: a match was found textually but no engine could
    /// bound it to an exact construct (design-specs §8.6). `content_buffer` is a
    /// verbatim window of lines around the hit.
    pub fn fallback(
        command: &str,
        target: &str,
        window: TextWindow,
        message: impl Into<String>,
    ) -> Self {
        let mut r = Record::new(command, target, Status::Fallback, "partial_resolution_fallback");
        r.file_path = Some(window.file_path);
        r.approximate_line = Some(window.approximate_line);
        r.window_before = Some(window.before);
        r.window_after = Some(window.after);
        r.content_buffer = Some(window.content_buffer);
        r.message = Some(message.into());
        r
    }

    /// Multiple resolved results (overloads shown with full content).
    pub fn multi_resolved(
        command: &str,
        target: &str,
        resolution_type: &str,
        resolutions: &[Resolution],
        total: usize,
    ) -> Self {
        let mut rec = Record::new(command, target, Status::Resolved, resolution_type);
        rec.engine = Some(resolutions[0].engine.clone());
        let results: Vec<ResolvedResult> = resolutions
            .iter()
            .map(|r| ResolvedResult {
                file_path: r.source_ref.file_path.to_string_lossy().into_owned(),
                start_line: r.source_ref.span.start_line,
                end_line: r.source_ref.span.end_line,
                start_byte: r.source_ref.span.start_byte,
                end_byte: r.source_ref.span.end_byte,
                content: String::from_utf8_lossy(&r.content_bytes).into_owned(),
                signature: r.symbol.signature.clone(),
                type_spelling: r.symbol.type_spelling.clone(),
                doc: r.symbol.doc.clone(),
                qualified_name: r.symbol.qualified_name.clone(),
            })
            .collect();
        let shown = results.len();
        rec.results = results;
        if total > shown {
            rec.message = Some(format!(
                "Showing {shown} of {total} matches (--max-results {shown})."
            ));
        } else {
            rec.message = Some(format!("Found {shown} matches."));
        }
        rec
    }

    /// find-refs location-only: dense list of reference locations.
    pub fn references(
        command: &str,
        target: &str,
        locations: Vec<RefLocation>,
        truncated: bool,
    ) -> Self {
        let mut rec = Record::new(command, target, Status::Resolved, "references");
        rec.message = Some(format!("Found {} references.", locations.len()));
        rec.locations = locations;
        rec.truncated = truncated;
        rec
    }

    /// find-refs --context: references with enclosing scope bodies.
    pub fn references_with_context(
        command: &str,
        target: &str,
        contexts: Vec<RefContext>,
        truncated: bool,
    ) -> Self {
        let mut rec = Record::new(command, target, Status::Resolved, "references_with_context");
        rec.message = Some(format!("Found {} references with context.", contexts.len()));
        rec.contexts = contexts;
        rec.truncated = truncated;
        rec
    }
}

/// A verbatim line-window around a textual hit, for the `fallback` rung
/// (design-specs §8.6). `before`/`after` are the actual line counts retained,
/// which can be smaller than the requested window near a file edge.
#[derive(Clone, Debug)]
pub struct TextWindow {
    pub file_path: String,
    pub approximate_line: usize,
    pub before: usize,
    pub after: usize,
    pub content_buffer: String,
}

/// Streams records to a sink in the selected [`Format`].
pub struct Writer<W: Write> {
    out: W,
    format: Format,
    legend: bool,
    /// Buffered records for bundle mode (jsonl streams directly).
    buffered: Vec<String>,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W, format: Format, legend: bool) -> Self {
        Writer {
            out,
            format,
            legend,
            buffered: Vec::new(),
        }
    }

    /// Emit a record (or buffer it, in bundle mode).
    pub fn write(&mut self, record: &Record) -> io::Result<()> {
        let line = serde_json::to_string(record)?;
        match self.format {
            Format::Jsonl => {
                self.out.write_all(line.as_bytes())?;
                self.out.write_all(b"\n")?;
            }
            Format::Bundle => self.buffered.push(line),
        }
        Ok(())
    }

    /// Flush any buffered output. For jsonl this is a no-op; for bundle it
    /// emits the single fenced block with an estimated-token footer.
    pub fn finish(mut self) -> io::Result<()> {
        if let Format::Bundle = self.format {
            if self.legend {
                writeln!(
                    self.out,
                    "// legend: each line is one JSON record; branch on `status` / `resolution_type`."
                )?;
            }
            writeln!(self.out, "```json")?;
            let mut chars = 0usize;
            for line in &self.buffered {
                chars += line.len();
                writeln!(self.out, "{line}")?;
            }
            writeln!(self.out, "```")?;
            // Rough heuristic until the real estimator lands (Phase 6).
            writeln!(self.out, "// ~{} tokens", chars / 4)?;
        }
        self.out.flush()
    }
}
