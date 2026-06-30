//! Flattens the JSON records produced by `find-def`/`find-decl`/`find-refs`
//! into the source-line rows the interactive browser navigates.
//!
//! A `Record` already covers every rung of the degradation ladder (resolved,
//! multi-resolved, ambiguous, fallback, not_found) with a different shape per
//! rung. [`collect_hits`] normalizes all of them into one flat [`Hit`] list;
//! [`expand_to_lines`] then explodes each hit's excerpt into individual
//! source [`Line`]s so every line in the tree is independently jumpable.

use std::collections::HashMap;
use std::fs;

use crate::interactive::tree::Line;
use crate::output::{Record, Status};

/// One resolved entity pulled out of a command's records: a definition, an
/// overload, a reference location, an ambiguous candidate, or a fallback
/// text window. Always anchored to a file and a line range.
pub struct Hit {
    pub target: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub label: String,
    pub qualified_name: Option<String>,
    /// Verbatim text covering `start_line..=end_line`, when the record
    /// already carried it. `None` means the browser reads the line(s) from
    /// disk itself, purely for display (never part of the wire output).
    pub excerpt: Option<String>,
}

/// Pull every navigable hit out of a batch of records, in record order.
pub fn collect_hits(records: &[Record]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for rec in records {
        match rec.status {
            Status::Resolved => collect_resolved(rec, &mut hits),
            Status::Ambiguous => collect_ambiguous(rec, &mut hits),
            Status::Fallback => collect_fallback(rec, &mut hits),
            Status::NotFound => {}
        }
    }
    hits
}

fn collect_resolved(rec: &Record, hits: &mut Vec<Hit>) {
    if !rec.results.is_empty() {
        for r in &rec.results {
            hits.push(Hit {
                target: rec.target.clone(),
                file: r.file_path.clone(),
                start_line: r.start_line,
                end_line: r.end_line,
                label: rec.resolution_type.clone(),
                qualified_name: r.qualified_name.clone(),
                excerpt: r.content.clone(),
            });
        }
    } else if !rec.locations.is_empty() {
        for loc in &rec.locations {
            hits.push(Hit {
                target: rec.target.clone(),
                file: loc.file.clone(),
                start_line: loc.line,
                end_line: loc.line,
                label: rec.resolution_type.clone(),
                qualified_name: None,
                excerpt: None,
            });
        }
    } else if !rec.contexts.is_empty() {
        for ctx in &rec.contexts {
            hits.push(Hit {
                target: rec.target.clone(),
                file: ctx.file.clone(),
                start_line: ctx.scope_start_line,
                end_line: ctx.scope_end_line,
                label: format!("{} (hit at L{})", rec.resolution_type, ctx.line),
                qualified_name: None,
                excerpt: Some(ctx.content.clone()),
            });
        }
    } else if let Some(file) = rec.file_path.clone() {
        let start = rec.start_line.unwrap_or(1);
        hits.push(Hit {
            target: rec.target.clone(),
            file,
            start_line: start,
            end_line: rec.end_line.unwrap_or(start),
            label: rec.resolution_type.clone(),
            qualified_name: rec.qualified_name.clone(),
            excerpt: rec.content.clone(),
        });
    }
}

fn collect_ambiguous(rec: &Record, hits: &mut Vec<Hit>) {
    for c in &rec.candidates {
        hits.push(Hit {
            target: rec.target.clone(),
            file: c.file_path.clone(),
            start_line: c.line,
            end_line: c.line,
            label: "ambiguous".to_string(),
            qualified_name: None,
            excerpt: c.snippet.clone(),
        });
    }
}

fn collect_fallback(rec: &Record, hits: &mut Vec<Hit>) {
    if !rec.windows.is_empty() {
        for w in &rec.windows {
            hits.push(Hit {
                target: rec.target.clone(),
                file: w.file_path.clone(),
                start_line: w.approximate_line.saturating_sub(w.window_before),
                end_line: w.approximate_line + w.window_after,
                label: "fallback".to_string(),
                qualified_name: None,
                excerpt: Some(w.content_buffer.clone()),
            });
        }
    } else if let (Some(file), Some(line)) = (rec.file_path.clone(), rec.approximate_line) {
        let before = rec.window_before.unwrap_or(0);
        let after = rec.window_after.unwrap_or(0);
        hits.push(Hit {
            target: rec.target.clone(),
            file,
            start_line: line.saturating_sub(before),
            end_line: line + after,
            label: "fallback".to_string(),
            qualified_name: None,
            excerpt: rec.content_buffer.clone(),
        });
    }
}

/// A small per-run cache of file contents, so re-reading the same file for
/// several excerpt-less hits (e.g. many find-refs locations in one file)
/// only touches disk once.
#[derive(Default)]
pub struct FileCache(HashMap<String, Vec<String>>);

impl FileCache {
    pub fn new() -> Self {
        Self::default()
    }

    fn line_text(&mut self, file: &str, line: usize) -> String {
        let lines = self.0.entry(file.to_string()).or_insert_with(|| {
            fs::read_to_string(file)
                .map(|s| s.lines().map(str::to_string).collect())
                .unwrap_or_default()
        });
        lines.get(line.saturating_sub(1)).cloned().unwrap_or_default()
    }
}

/// Explode every hit's excerpt into one [`Line`] per source line, anchoring
/// the first line of each hit (it carries the summary label; the rest are
/// dimmed detail lines) but leaving every line independently jumpable.
pub fn expand_to_lines(hits: &[Hit], cache: &mut FileCache) -> Vec<Line> {
    let mut lines = Vec::new();
    for hit in hits {
        let label = describe(hit);
        match &hit.excerpt {
            Some(text) => {
                let body: Vec<&str> = text.split('\n').collect();
                for (i, text_line) in body.iter().enumerate() {
                    lines.push(Line {
                        file: hit.file.clone(),
                        line: hit.start_line + i,
                        text: (*text_line).to_string(),
                        anchor: i == 0,
                        label: if i == 0 { Some(label.clone()) } else { None },
                    });
                }
            }
            None => {
                let text = cache.line_text(&hit.file, hit.start_line);
                lines.push(Line {
                    file: hit.file.clone(),
                    line: hit.start_line,
                    text,
                    anchor: true,
                    label: Some(label),
                });
            }
        }
    }
    lines
}

fn describe(hit: &Hit) -> String {
    let mut s = format!("[{}] {}", hit.target, hit.label);
    if let Some(q) = &hit.qualified_name {
        s.push(' ');
        s.push_str(q);
    }
    if hit.start_line == hit.end_line {
        s.push_str(&format!("  L{}", hit.start_line));
    } else {
        s.push_str(&format!("  L{}-{}", hit.start_line, hit.end_line));
    }
    s
}
