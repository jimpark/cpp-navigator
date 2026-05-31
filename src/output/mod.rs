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
    /// Emit ANSI color escapes in human mode (true only when stdout is a TTY).
    colors: bool,
    /// Count of records written, used to insert separators in human mode.
    record_count: usize,
    /// Buffered records for bundle mode (jsonl streams directly).
    buffered: Vec<String>,
}

impl<W: Write> Writer<W> {
    pub fn new(out: W, format: Format, legend: bool, colors: bool) -> Self {
        Writer {
            out,
            format,
            legend,
            colors,
            record_count: 0,
            buffered: Vec::new(),
        }
    }

    /// Emit a record (or buffer it, in bundle mode).
    pub fn write(&mut self, record: &Record) -> io::Result<()> {
        match self.format {
            Format::Jsonl => {
                let line = serde_json::to_string(record)?;
                self.out.write_all(line.as_bytes())?;
                self.out.write_all(b"\n")?;
            }
            Format::Bundle => {
                let line = serde_json::to_string(record)?;
                self.buffered.push(line);
            }
            Format::Human => {
                if self.record_count > 0 {
                    self.out.write_all(b"\n")?;
                }
                let rendered = render_human(record, self.colors);
                self.out.write_all(rendered.as_bytes())?;
                self.record_count += 1;
            }
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
            let mut total_bytes = 0usize;
            for line in &self.buffered {
                total_bytes += line.len();
                writeln!(self.out, "{line}")?;
            }
            writeln!(self.out, "```")?;
            writeln!(self.out, "// ~{} tokens", estimate_tokens(total_bytes))?;
        }
        self.out.flush()
    }
}

/// Render a [`Record`] as human-readable text.
///
/// When `colors` is true, ANSI escape codes are emitted for bold/color
/// highlights. When false, the output is plain text suitable for piping.
fn render_human(record: &Record, colors: bool) -> String {
    let bold   = |s: &str| if colors { format!("\x1b[1m{s}\x1b[0m")    } else { s.to_string() };
    let green  = |s: &str| if colors { format!("\x1b[32;1m{s}\x1b[0m") } else { s.to_string() };
    let yellow = |s: &str| if colors { format!("\x1b[33;1m{s}\x1b[0m") } else { s.to_string() };
    let red    = |s: &str| if colors { format!("\x1b[31;1m{s}\x1b[0m") } else { s.to_string() };
    let dim    = |s: &str| if colors { format!("\x1b[2m{s}\x1b[0m")    } else { s.to_string() };
    let cyan   = |s: &str| if colors { format!("\x1b[36m{s}\x1b[0m")   } else { s.to_string() };

    let mut out = String::new();

    // Header line: "find-def: MySymbol  RESOLVED (function_definition)  via tree-sitter"
    let cmd_target = format!("{}: {}", record.command, record.target);
    let status_tag = match record.status {
        Status::Resolved  => green("RESOLVED"),
        Status::Ambiguous => yellow("AMBIGUOUS"),
        Status::Fallback  => yellow("FALLBACK"),
        Status::NotFound  => red("NOT FOUND"),
    };
    let rtype = dim(&format!("({})", record.resolution_type));
    let engine_suffix = match &record.engine {
        Some(e) => format!("  {}", dim(&format!("via {e}"))),
        None    => String::new(),
    };
    out += &format!("{}  {} {}{}\n", bold(&cmd_target), status_tag, rtype, engine_suffix);

    match record.status {
        Status::Resolved => {
            if !record.results.is_empty() {
                // Multi-resolved: overloads shown in full.
                if let Some(msg) = &record.message {
                    out += &format!("{}\n", dim(msg));
                }
                for (i, r) in record.results.iter().enumerate() {
                    let loc = format!("{}:{}-{}", r.file_path, r.start_line, r.end_line);
                    out += &format!("\n[{}] {}\n", i + 1, cyan(&loc));
                    if let Some(sig) = &r.signature {
                        out += &format!("    {}\n", dim(sig));
                    }
                    out += "\n";
                    for line in r.content.lines() {
                        out += &format!("    {line}\n");
                    }
                }
            } else if !record.contexts.is_empty() {
                // find-refs --context: reference + enclosing scope body.
                if let Some(msg) = &record.message {
                    out += &format!("{}\n", dim(msg));
                }
                for ctx in &record.contexts {
                    let loc = format!("{}:{}", ctx.file, ctx.line);
                    let scope = format!("scope lines {}-{}", ctx.scope_start_line, ctx.scope_end_line);
                    out += &format!("\n{}  {}\n\n", cyan(&loc), dim(&scope));
                    for line in ctx.content.lines() {
                        out += &format!("    {line}\n");
                    }
                }
                if record.truncated {
                    out += &format!("{}\n", dim("(truncated — more results omitted)"));
                }
            } else if !record.locations.is_empty() {
                // find-refs location-only: dense file:line list.
                if let Some(msg) = &record.message {
                    out += &format!("{}\n", dim(msg));
                }
                for loc in &record.locations {
                    out += &format!("  {}:{}\n", cyan(&loc.file), loc.line);
                }
                if record.truncated {
                    out += &format!("  {}\n", dim("(truncated — more results omitted)"));
                }
            } else if let Some(file) = &record.file_path {
                // Single resolved definition or declaration.
                let loc = format!(
                    "{}:{}-{}",
                    file,
                    record.start_line.unwrap_or(0),
                    record.end_line.unwrap_or(0),
                );
                out += &format!("{}\n", cyan(&loc));
                if let Some(sig) = &record.signature {
                    out += &format!("Signature: {sig}\n");
                }
                if let Some(doc) = &record.doc {
                    for line in doc.lines() {
                        out += &format!("{}\n", dim(line));
                    }
                }
                out += "\n";
                if let Some(content) = &record.content {
                    for line in content.lines() {
                        out += &format!("    {line}\n");
                    }
                }
                if record.truncated {
                    out += &format!("{}\n", dim("(truncated)"));
                }
                if let Some(msg) = &record.message {
                    out += &format!("{}\n", dim(msg));
                }
            }
        }
        Status::Ambiguous => {
            if let Some(msg) = &record.message {
                out += &format!("{}\n", dim(msg));
            }
            for cand in &record.candidates {
                let loc = format!("{}:{}", cand.file_path, cand.line);
                match &cand.snippet {
                    Some(s) => out += &format!("  {}  {}\n", cyan(&loc), dim(s)),
                    None    => out += &format!("  {}\n", cyan(&loc)),
                }
            }
        }
        Status::Fallback => {
            if let (Some(file), Some(approx)) = (&record.file_path, record.approximate_line) {
                let loc = format!("{file}  ~line {approx}");
                out += &format!("{}\n", cyan(&loc));
            }
            if let Some(msg) = &record.message {
                out += &format!("{}\n", dim(msg));
            }
            if let Some(buf) = &record.content_buffer {
                out += "\n";
                for line in buf.lines() {
                    out += &format!("    {line}\n");
                }
            }
        }
        Status::NotFound => {
            if let Some(msg) = &record.message {
                out += &format!("{msg}\n");
            }
        }
    }

    if record.budget_trimmed {
        out += &format!("{}\n", dim("(budget trimmed)"));
    }

    out
}

/// Estimate token count from byte length.
///
/// Uses a conservative heuristic: JSON + C++ source averages ~3.5 chars per
/// token for typical LLM tokenizers (cl100k/GPT-4 family). We use 3.3 to err
/// on the side of overestimating (safer for budget enforcement).
pub fn estimate_tokens(bytes: usize) -> usize {
    // Integer math: bytes * 10 / 33 ≈ bytes / 3.3
    (bytes * 10).div_ceil(33)
}

/// Apply `--budget` selection-only trim to a set of records.
///
/// Trims by dropping records from the end until the estimated token count is
/// within budget. For records with `contexts` or `results` arrays, those
/// arrays are trimmed first before dropping whole records. The `content` and
/// `content_buffer` payload bytes are **never** edited (§8.4).
///
/// Returns the (possibly trimmed) records with `budget_trimmed` set on the
/// last record if trimming occurred.
pub fn apply_budget(mut records: Vec<Record>, budget: usize) -> Vec<Record> {
    let total_tokens: usize = records
        .iter()
        .map(|r| estimate_tokens(serde_json::to_string(r).unwrap_or_default().len()))
        .sum();

    if total_tokens <= budget {
        return records;
    }

    // Strategy: trim arrays on large records first (contexts, locations, results),
    // then drop whole records from the end as a last resort.
    loop {
        let current: usize = records
            .iter()
            .map(|r| estimate_tokens(serde_json::to_string(r).unwrap_or_default().len()))
            .sum();
        if current <= budget || records.is_empty() {
            break;
        }

        // Find the largest array and trim it.
        let mut trimmed_any = false;
        for rec in records.iter_mut().rev() {
            if rec.contexts.len() > 1 {
                rec.contexts.pop();
                rec.budget_trimmed = true;
                trimmed_any = true;
                break;
            }
            if rec.locations.len() > 10 {
                rec.locations.truncate(rec.locations.len() / 2);
                rec.budget_trimmed = true;
                trimmed_any = true;
                break;
            }
            if rec.results.len() > 1 {
                rec.results.pop();
                rec.budget_trimmed = true;
                trimmed_any = true;
                break;
            }
            if rec.candidates.len() > 3 {
                rec.candidates.truncate(rec.candidates.len() / 2);
                rec.budget_trimmed = true;
                trimmed_any = true;
                break;
            }
        }
        if !trimmed_any {
            // No arrays left to trim — drop the last record.
            if records.len() > 1 {
                records.pop();
                if let Some(last) = records.last_mut() {
                    last.budget_trimmed = true;
                    last.message = Some(
                        "Budget trimmed: some results were dropped to fit token budget.".to_string(),
                    );
                }
            } else {
                // Single record left — mark it and stop.
                records[0].budget_trimmed = true;
                records[0].message = Some(
                    "Budget trimmed: result was reduced to fit token budget.".to_string(),
                );
                break;
            }
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_reasonable() {
        // 100 chars should be ~30 tokens (100/3.3)
        let t = estimate_tokens(100);
        assert!((28..=35).contains(&t), "got {t}");
    }

    #[test]
    fn estimate_tokens_zero() {
        assert_eq!(estimate_tokens(0), 0);
    }

    #[test]
    fn budget_no_trim_when_under() {
        let rec = Record::not_found("find-def", "foo");
        let records = vec![rec];
        let result = apply_budget(records.clone(), 10000);
        assert_eq!(result.len(), 1);
        assert!(!result[0].budget_trimmed);
    }

    #[test]
    fn budget_trims_large_locations() {
        let mut rec = Record::not_found("find-refs", "foo");
        rec.locations = (0..200)
            .map(|i| RefLocation {
                file: format!("/path/to/file_{i}.cpp"),
                line: i,
            })
            .collect();
        let original_len = rec.locations.len();
        let result = apply_budget(vec![rec], 50);
        assert!(result[0].locations.len() < original_len);
        assert!(result[0].budget_trimmed);
    }

    #[test]
    fn budget_drops_records_as_last_resort() {
        let records: Vec<Record> = (0..10)
            .map(|i| {
                let mut r = Record::not_found("find-def", &format!("target_{i}"));
                r.content = Some("x".repeat(500));
                r
            })
            .collect();
        let result = apply_budget(records, 100);
        assert!(result.len() < 10);
        assert!(result.last().unwrap().budget_trimmed);
    }

    #[test]
    fn bundle_writer_emits_token_count() {
        let mut buf = Vec::new();
        {
            let mut w = Writer::new(&mut buf, Format::Bundle, false, false);
            let rec = Record::not_found("find-def", "foo");
            w.write(&rec).unwrap();
            w.finish().unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("```json"));
        assert!(out.contains("// ~"));
        assert!(out.contains("tokens"));
    }

    #[test]
    fn bundle_writer_with_legend() {
        let mut buf = Vec::new();
        {
            let mut w = Writer::new(&mut buf, Format::Bundle, true, false);
            let rec = Record::not_found("find-def", "foo");
            w.write(&rec).unwrap();
            w.finish().unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("legend:"));
    }

    /// JSON Schema structural validation: every Record emitted via serde_json
    /// must contain the required envelope fields and a valid schema_version.
    #[test]
    fn schema_version_1_0_envelope_fields() {
        // Test all status variants to ensure schema_version + envelope always present.
        let records = vec![
            Record::not_found("find-def", "missing"),
            Record::resolved("find-def", "foo", "definition", &crate::model::Resolution {
                symbol: crate::model::Symbol {
                    name: "foo".to_string(),
                    qualified_name: Some("ns::foo".to_string()),
                    kind: crate::model::Kind::Function,
                    signature: None,
                    type_spelling: None,
                    doc: None,
                },
                source_ref: crate::model::SourceRef {
                    file_path: std::path::PathBuf::from("/tmp/a.cpp"),
                    span: crate::model::Span {
                        start_byte: 0,
                        end_byte: 10,
                        start_line: 1,
                        end_line: 1,
                        start_col: 0,
                        end_col: 10,
                    },
                },
                content_bytes: b"int foo() {}".to_vec(),
                engine: "tree-sitter".to_string(),
                confidence: 0.9,
                status: crate::model::Status::Resolved,
            }),
            Record::ambiguous("find-def", "bar", vec![
                Candidate { file_path: "/a.h".to_string(), line: 1, snippet: None },
                Candidate { file_path: "/b.h".to_string(), line: 2, snippet: None },
            ]),
            Record::references("find-refs", "baz", vec![
                RefLocation { file: "/x.cpp".to_string(), line: 10 },
            ], false),
        ];

        for rec in &records {
            let json_str = serde_json::to_string(rec).unwrap();
            let val: serde_json::Value = serde_json::from_str(&json_str).unwrap();
            let obj = val.as_object().unwrap();

            // Required envelope fields (design-specs §8).
            assert_eq!(obj["schema_version"].as_str().unwrap(), "1.0");
            assert_eq!(obj["tool"].as_str().unwrap(), "cpp-navigator");
            assert!(obj.contains_key("command"), "missing 'command'");
            assert!(obj.contains_key("target"), "missing 'target'");
            assert!(obj.contains_key("status"), "missing 'status'");
            assert!(obj.contains_key("resolution_type"), "missing 'resolution_type'");

            // Status must be one of the allowed values.
            let status = obj["status"].as_str().unwrap();
            assert!(
                ["resolved", "ambiguous", "fallback", "not_found"].contains(&status),
                "invalid status: {status}"
            );
        }
    }

    /// Validates that omission-based serialization works: fields that are None
    /// or empty Vec don't appear in the JSON output.
    #[test]
    fn schema_omits_empty_fields() {
        let rec = Record::not_found("find-def", "gone");
        let json_str = serde_json::to_string(&rec).unwrap();
        let val: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = val.as_object().unwrap();

        // These should be absent for a not_found record.
        assert!(!obj.contains_key("file_path"));
        assert!(!obj.contains_key("content"));
        assert!(!obj.contains_key("start_line"));
        assert!(!obj.contains_key("end_line"));
        assert!(!obj.contains_key("candidates"));
        assert!(!obj.contains_key("locations"));
        assert!(!obj.contains_key("contexts"));
        assert!(!obj.contains_key("results"));
        assert!(!obj.contains_key("truncated"));
        assert!(!obj.contains_key("budget_trimmed"));
    }
}
