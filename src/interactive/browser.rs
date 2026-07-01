//! The interactive tree browser: a full-screen, modal picker over the lines
//! produced by [`crate::interactive::hits`]. Navigate with arrows or
//! `j`/`k`/`g`/`G`/`h`/`l`, hit Enter to open the line under the cursor —
//! handed to a detected IDE's integrated terminal when running inside one,
//! or spawned as a configured terminal editor otherwise.

use std::collections::HashSet;
use std::io::{self, Write};
use std::process::Command;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};

use crate::interactive::ide::Ide;
use crate::interactive::tree::{self, Line, Node, Row};

enum Mode {
    Browse,
    Filter,
}

fn enter_screen() -> io::Result<()> {
    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, Hide)
}

fn leave_screen() {
    let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

/// Restores the terminal on drop (including on panic unwind) so a crash
/// never leaves the user's terminal stuck in raw/alternate-screen mode.
struct ScreenGuard;

impl Drop for ScreenGuard {
    fn drop(&mut self) {
        leave_screen();
    }
}

pub struct Browser {
    title: String,
    all_lines: Vec<Line>,
    query: String,
    mode: Mode,
    expanded: HashSet<String>,
    /// The built tree, cached alongside the query that produced it. Folding
    /// only toggles `expanded` (which `build_visible` reads directly), so the
    /// tree is reused across expand/collapse and rebuilt only when the filter
    /// query changes — avoiding a clone of every line plus a full re-weave.
    node_cache: Option<(String, Node)>,
    rows: Vec<Row>,
    /// Header tallies, recomputed only in `rebuild` rather than on every
    /// frame, so navigation doesn't pay an O(rows) pass per keypress.
    line_count: usize,
    file_count: usize,
    cursor: usize,
    cur_id: Option<String>,
    top: usize,
    status: String,
    jump_active: bool,
    jump_buf: String,
    quit: bool,
    ide: Option<Ide>,
    editor_argv: Vec<String>,
    line_template: String,
}

impl Browser {
    pub fn new(
        title: String,
        lines: Vec<Line>,
        ide: Option<Ide>,
        editor_argv: Vec<String>,
        line_template: String,
    ) -> Self {
        let mut expanded = HashSet::new();
        let root = tree::build_tree(&lines);
        tree::all_folder_paths(&root, &mut expanded);
        let mut browser = Browser {
            title,
            all_lines: lines,
            query: String::new(),
            mode: Mode::Browse,
            expanded,
            node_cache: None,
            rows: Vec::new(),
            line_count: 0,
            file_count: 0,
            cursor: 0,
            cur_id: None,
            top: 0,
            status: String::new(),
            jump_active: false,
            jump_buf: String::new(),
            quit: false,
            ide,
            editor_argv,
            line_template,
        };
        browser.rebuild();
        browser
    }

    fn filtered_lines(&self) -> Vec<Line> {
        if self.query.is_empty() {
            return self.all_lines.clone();
        }
        let q = self.query.to_lowercase();
        self.all_lines
            .iter()
            .filter(|l| {
                l.file.to_lowercase().contains(&q)
                    || l.text.to_lowercase().contains(&q)
                    || l.label.as_deref().unwrap_or("").to_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    }

    fn build_node(&self) -> Node {
        tree::build_tree(&self.filtered_lines())
    }

    fn rebuild(&mut self) {
        // Rebuild the tree only when the filter query changed; a fold/unfold
        // leaves the tree identical and merely re-flattens it below.
        let reuse = matches!(&self.node_cache, Some((q, _)) if *q == self.query);
        if !reuse {
            let node = self.build_node();
            self.node_cache = Some((self.query.clone(), node));
        }
        self.rows = {
            let node = &self.node_cache.as_ref().unwrap().1;
            tree::build_visible(node, &self.expanded)
        };
        self.retally();
        if let Some(id) = &self.cur_id {
            if let Some(i) = self.rows.iter().position(|r| &r.id == id) {
                self.cursor = i;
            } else {
                self.cursor = self.first_line_index();
            }
        } else {
            self.cursor = self.first_line_index();
        }
        self.cursor = if self.rows.is_empty() { 0 } else { self.cursor.min(self.rows.len() - 1) };
        self.cur_id = self.rows.get(self.cursor).map(|r| r.id.clone());
    }

    /// Recompute the header tallies (line count and distinct file count)
    /// from the current rows. Called from `rebuild` only — never per frame.
    fn retally(&mut self) {
        self.line_count = self.rows.iter().filter(|r| !r.is_folder).count();
        let files: HashSet<&str> =
            self.rows.iter().filter_map(|r| r.line.as_ref()).map(|l| l.file.as_str()).collect();
        self.file_count = files.len();
    }

    fn first_line_index(&self) -> usize {
        self.rows.iter().position(|r| !r.is_folder).unwrap_or(0)
    }

    fn sync_id(&mut self) {
        self.cur_id = self.rows.get(self.cursor).map(|r| r.id.clone());
    }

    fn move_cursor(&mut self, delta: i64) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as i64;
        let c = (self.cursor as i64 + delta).clamp(0, n - 1);
        self.cursor = c as usize;
        self.sync_id();
    }

    fn move_to(&mut self, index: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.cursor = index.min(self.rows.len() - 1);
        self.sync_id();
    }

    /// Open the folder at `index` incrementally: record it as expanded and
    /// splice its subtree into `rows`, rather than rebuilding the whole list.
    /// Only the header tallies and cursor id need refreshing afterwards.
    fn expand_at(&mut self, index: usize) {
        let path = self.rows[index].id.trim_start_matches("F:").to_string();
        self.expanded.insert(path);
        tree::splice_expand(&mut self.rows, index, &self.node_cache.as_ref().unwrap().1, &self.expanded);
        self.retally();
        self.sync_id();
    }

    /// Close the folder at `index` incrementally: forget it as expanded and
    /// splice its subtree back out of `rows`.
    fn collapse_at(&mut self, index: usize) {
        let path = self.rows[index].id.trim_start_matches("F:").to_string();
        self.expanded.remove(&path);
        tree::splice_collapse(&mut self.rows, index);
        self.retally();
        self.sync_id();
    }

    fn expand(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else { return };
        if !row.is_folder {
            return;
        }
        if row.expanded {
            self.move_cursor(1);
        } else {
            self.expand_at(self.cursor);
        }
    }

    fn collapse(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else { return };
        if row.is_folder && row.expanded {
            self.collapse_at(self.cursor);
            return;
        }
        let path = if row.is_folder {
            row.id.trim_start_matches("F:").to_string()
        } else {
            // A line row's id is "L:<dir>/<file>\0<key>"; its parent folder
            // is the file, i.e. everything before the NUL separator.
            row.id.trim_start_matches("L:").split('\0').next().unwrap_or("").to_string()
        };
        let Some((parent, _)) = path.rsplit_once('/') else { return };
        let target = format!("F:{parent}");
        if let Some(i) = self.rows.iter().position(|r| r.id == target) {
            self.move_to(i);
        }
    }

    fn file_row_indices(&self) -> Vec<usize> {
        self.rows.iter().enumerate().filter(|(_, r)| r.is_folder && r.is_file_node).map(|(i, _)| i).collect()
    }

    /// Row index of the file row that owns `rows[idx]` (itself, if it
    /// already is one), or `None` when the row sits on a plain directory
    /// folder rather than inside some file's subtree.
    fn owning_file_index(&self, idx: usize) -> Option<usize> {
        let row = self.rows.get(idx)?;
        if row.is_folder && row.is_file_node {
            return Some(idx);
        }
        let depth = row.depth;
        for i in (0..idx).rev() {
            if self.rows[i].depth < depth {
                let parent = &self.rows[i];
                return (parent.is_folder && parent.is_file_node).then_some(i);
            }
        }
        None
    }

    /// Land the cursor on the first line under the file row at `file_idx`,
    /// expanding it first if it's currently folded.
    fn goto_file(&mut self, file_idx: usize) {
        let idx = file_idx;
        if !self.rows[idx].expanded {
            // Splicing keeps the folder row at the same index, so no lookup
            // of the row's new position is needed afterward.
            self.expand_at(idx);
        }
        let mut target = idx;
        if target + 1 < self.rows.len() && self.rows[target + 1].depth > self.rows[target].depth {
            target += 1;
        }
        self.move_to(target);
    }

    /// Jump to the first line of the next file; clamps at the last file
    /// with a status message rather than wrapping.
    fn next_file(&mut self) {
        let files = self.file_row_indices();
        if files.is_empty() {
            return;
        }
        let anchor = self.owning_file_index(self.cursor).unwrap_or(self.cursor);
        match files.into_iter().find(|&i| i > anchor) {
            Some(nxt) => self.goto_file(nxt),
            None => self.status = "already on the last file".to_string(),
        }
    }

    /// Jump to the first line of the previous file; clamps at the first
    /// file with a status message rather than wrapping.
    fn prev_file(&mut self) {
        let files = self.file_row_indices();
        if files.is_empty() {
            return;
        }
        let anchor = self.owning_file_index(self.cursor).unwrap_or(self.cursor);
        match files.into_iter().rev().find(|&i| i < anchor) {
            Some(prv) => self.goto_file(prv),
            None => self.status = "already on the first file".to_string(),
        }
    }

    fn jump_to_number(&mut self) {
        if self.jump_buf.is_empty() {
            return;
        }
        let n: usize = self.jump_buf.parse().unwrap_or(0);
        let mut target = None;
        for (i, row) in self.rows.iter().enumerate() {
            if let Some(num) = row.number {
                target = Some(i);
                if num >= n {
                    break;
                }
            }
        }
        if let Some(i) = target {
            self.move_to(i);
        }
    }

    fn open_current(&mut self) {
        let Some(row) = self.rows.get(self.cursor) else { return };
        if row.is_folder {
            if row.expanded {
                self.collapse_at(self.cursor);
            } else {
                self.expand_at(self.cursor);
            }
            return;
        }
        let line = row.line.clone().unwrap();
        // Records carry root-relative paths with mixed separators (e.g.
        // "src\widget.cpp"); IDE hand-offs and spawned editors all need an
        // absolute path, so resolve it once here against the cwd.
        let abs = absolute_path(&line.file);
        let abs_str = abs.to_string_lossy();
        if let Some(ide) = &self.ide {
            match ide.open(&abs_str, line.line, 1) {
                Ok(()) => self.status = format!("opened {}:{} in {}", line.file, line.line, ide.label()),
                Err(_) => self.status = ide.open_error(),
            }
            return;
        }
        let argv = super::editor::open_args(&self.editor_argv, &self.line_template, &abs_str, line.line, 1);
        let Some((cmd, args)) = argv.split_first() else { return };
        leave_screen();
        let result = Command::new(cmd).args(args).status();
        let _ = enter_screen();
        self.status = match result {
            Ok(_) => format!("opened {}:{}", line.file, line.line),
            Err(e) => format!("could not open editor {cmd}: {e}"),
        };
    }

    pub fn run(&mut self) -> io::Result<()> {
        enter_screen()?;
        let _guard = ScreenGuard;
        while !self.quit {
            self.render()?;
            match event::read()? {
                Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                        break;
                    }
                    self.handle_key(key.code);
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, code: KeyCode) {
        self.status.clear();
        match self.mode {
            Mode::Browse => self.handle_browse(code),
            Mode::Filter => self.handle_filter(code),
        }
    }

    fn handle_browse(&mut self, code: KeyCode) {
        if self.jump_active {
            match code {
                KeyCode::Char(':') => {
                    self.jump_buf.clear();
                    return;
                }
                KeyCode::Backspace => {
                    self.jump_buf.pop();
                    self.jump_to_number();
                    return;
                }
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    self.jump_buf.push(c);
                    self.jump_to_number();
                    return;
                }
                _ => {
                    self.jump_active = false;
                    if matches!(code, KeyCode::Enter | KeyCode::Esc) {
                        return;
                    }
                }
            }
        }
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::Home | KeyCode::Char('g') => self.move_to(0),
            KeyCode::End | KeyCode::Char('G') => {
                let last = self.rows.len().saturating_sub(1);
                self.move_to(last);
            }
            KeyCode::Left | KeyCode::Char('h') => self.collapse(),
            KeyCode::Right | KeyCode::Char('l') => self.expand(),
            KeyCode::Char('n') => self.next_file(),
            KeyCode::Char('p') => self.prev_file(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char(':') => {
                self.jump_active = true;
                self.jump_buf.clear();
            }
            KeyCode::Enter => self.open_current(),
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            _ => {}
        }
    }

    fn handle_filter(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => {
                self.query.clear();
                self.mode = Mode::Browse;
                self.cur_id = None;
                self.rebuild();
            }
            KeyCode::Enter => self.mode = Mode::Browse,
            KeyCode::Backspace => {
                self.query.pop();
                self.cur_id = None;
                self.rebuild();
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.cur_id = None;
                self.rebuild();
            }
            _ => {}
        }
    }

    fn render(&mut self) -> io::Result<()> {
        let (cols, term_rows) = terminal::size().unwrap_or((80, 24));
        let (cols, term_rows) = (cols as usize, term_rows as usize);
        let area = term_rows.saturating_sub(4).max(1);

        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + area {
            self.top = self.cursor + 1 - area;
        }
        let max_top = self.rows.len().saturating_sub(area);
        self.top = self.top.min(max_top);

        let mut out = io::stdout();
        queue!(out, MoveTo(0, 0))?;

        let mode_label = match self.mode {
            Mode::Browse => "BROWSE",
            Mode::Filter => "FILTER",
        };
        let header = format!(
            " {}    [{}]    {} line{} in {} file{}",
            self.title,
            mode_label,
            self.line_count,
            if self.line_count == 1 { "" } else { "s" },
            self.file_count,
            if self.file_count == 1 { "" } else { "s" },
        );
        print_line(&mut out, &truncate(&header, cols), cols, None, Some(Attribute::Bold))?;
        print_line(&mut out, &"-".repeat(cols), cols, None, None)?;

        let end = (self.top + area).min(self.rows.len());
        let window = &self.rows[self.top..end];
        for (i, row) in window.iter().enumerate() {
            let idx = self.top + i;
            let text = truncate(&row_text(row), cols);
            if idx == self.cursor {
                print_line(&mut out, &text, cols, None, Some(Attribute::Reverse))?;
            } else {
                print_line(&mut out, &text, cols, row_color(row), None)?;
            }
        }
        for _ in window.len()..area {
            queue!(out, Clear(ClearType::UntilNewLine), Print("\r\n"))?;
        }

        print_line(&mut out, &"-".repeat(cols), cols, None, None)?;
        // Bottom row: print the footer with NO trailing newline. On Windows
        // consoles a newline on the last row scrolls the whole buffer up one
        // line, which the next frame's MoveTo(0,0) snaps back — a visible
        // bounce as you navigate. Parking the cursor on the footer (then
        // clearing anything below) avoids it. Clear-to-end also wipes any
        // stale characters left on the footer line itself.
        let footer = truncate(&self.footer_text(), cols);
        queue!(out, Print(&footer), Clear(ClearType::FromCursorDown))?;
        out.flush()
    }

    fn footer_text(&self) -> String {
        match self.mode {
            Mode::Filter => format!(" filter: {}    Enter apply * Esc clear", self.query),
            Mode::Browse => {
                let base = " j/k move * n/p file * Enter open * h/l fold * / filter * :N jump * g/G top/bottom * q quit";
                if self.status.is_empty() {
                    base.to_string()
                } else {
                    format!("{base}    {}", self.status)
                }
            }
        }
    }
}

fn truncate(s: &str, cols: usize) -> String {
    s.chars().take(cols).collect()
}

/// Resolve a (possibly root-relative, mixed-separator) record path to an
/// absolute path, without touching the filesystem or adding Windows'
/// `\\?\` extended-length prefix. Falls back to the path as-given if the
/// cwd can't be determined.
fn absolute_path(rel: &str) -> std::path::PathBuf {
    std::path::absolute(rel).unwrap_or_else(|_| std::path::PathBuf::from(rel))
}

fn row_color(row: &Row) -> Option<Color> {
    if row.is_folder {
        return Some(if row.is_file_node { Color::Cyan } else { Color::Blue });
    }
    if !row.line.as_ref().is_some_and(|l| l.anchor) {
        return Some(Color::DarkGrey);
    }
    None
}

fn print_line(
    out: &mut impl Write,
    text: &str,
    cols: usize,
    fg: Option<Color>,
    attr: Option<Attribute>,
) -> io::Result<()> {
    if let Some(c) = fg {
        queue!(out, SetForegroundColor(c))?;
    }
    if let Some(a) = attr {
        queue!(out, SetAttribute(a))?;
    }
    let pad = cols.saturating_sub(text.chars().count());
    queue!(out, Print(text), Print(" ".repeat(pad)))?;
    if fg.is_some() || attr.is_some() {
        queue!(out, SetAttribute(Attribute::Reset))?;
    }
    queue!(out, Clear(ClearType::UntilNewLine), Print("\r\n"))
}

fn row_text(row: &Row) -> String {
    let indent = "  ".repeat(row.depth);
    if row.is_folder {
        let arrow = if row.expanded { "\u{25be}" } else { "\u{25b8}" };
        if row.is_file_node {
            format!("      {indent}{arrow} {} ({})", row.seg, row.child_count)
        } else {
            format!("      {indent}{arrow} {}/", row.seg)
        }
    } else {
        let line = row.line.as_ref().unwrap();
        let num = row.number.unwrap_or(0);
        let head = format!("{num:>4}  {:>5}  ", line.line);
        match &line.label {
            Some(l) => format!("{head}{indent}{l}  | {}", line.text),
            None => format!("{head}{indent}{}", line.text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(file: &str, n: usize, anchor: bool) -> Line {
        Line {
            file: file.to_string(),
            line: n,
            text: format!("{file}:{n}"),
            anchor,
            label: anchor.then(|| format!("[{file}] hit")),
        }
    }

    fn three_file_browser() -> Browser {
        let lines = vec![
            line("a.cpp", 1, true),
            line("a.cpp", 2, false),
            line("b.cpp", 1, true),
            line("b.cpp", 2, false),
            line("c.cpp", 1, true),
        ];
        Browser::new("t".to_string(), lines, None, vec!["true".to_string()], "{file}".to_string())
    }

    fn cursor_file(b: &Browser) -> &str {
        &b.rows[b.cursor].line.as_ref().unwrap().file
    }

    #[test]
    fn next_prev_file_clamp_without_wrapping() {
        let mut b = three_file_browser();
        assert_eq!(cursor_file(&b), "a.cpp");

        b.prev_file();
        assert_eq!(b.status, "already on the first file");
        assert_eq!(cursor_file(&b), "a.cpp");

        b.next_file();
        assert_eq!(cursor_file(&b), "b.cpp");
        assert_eq!(b.rows[b.cursor].line.as_ref().unwrap().line, 1);

        b.next_file();
        assert_eq!(cursor_file(&b), "c.cpp");

        b.next_file();
        assert_eq!(b.status, "already on the last file");
        assert_eq!(cursor_file(&b), "c.cpp");

        b.prev_file();
        assert_eq!(cursor_file(&b), "b.cpp");
        b.prev_file();
        assert_eq!(cursor_file(&b), "a.cpp");
        b.prev_file();
        assert_eq!(b.status, "already on the first file");
        assert_eq!(cursor_file(&b), "a.cpp");
    }

    #[test]
    fn next_file_expands_a_folded_file() {
        let mut b = three_file_browser();
        b.expanded.remove("b.cpp");
        b.rebuild();
        assert_eq!(cursor_file(&b), "a.cpp");

        b.next_file();
        assert!(b.expanded.contains("b.cpp"), "next_file should expand the folded file");
        assert_eq!(cursor_file(&b), "b.cpp");
    }
}
