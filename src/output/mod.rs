//! Output serialization (design-specs §8).
//!
//! Defines the stable wire format: a common envelope on every record plus
//! status-specific fields, emitted as JSONL or wrapped in a paste-able bundle.
//! Empty/irrelevant fields are omitted entirely (token economy via omission,
//! never by altering payload bytes — design-specs §8.1, §8.4).

use std::io::{self, Write};

use serde::Serialize;

use crate::cli::Format;

pub const SCHEMA_VERSION: &str = "1.0";
pub const TOOL: &str = "cpp-navigator";

/// Wire-level status (snake_case on the wire to match `resolution_type`).
#[derive(Clone, Copy, Debug, Serialize)]
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
