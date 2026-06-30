//! Folder/file/line tree used by the interactive browser, mirroring the
//! collapsible tree of a line-oriented grep picker: directories and files
//! nest by path segment, and each matching line sits under its file as a
//! leaf — except here a "match" is one line of a resolved hit's excerpt.

use std::collections::{HashMap, HashSet};

/// One source line shown in the tree. `anchor` marks the first line of a
/// hit (bold, carries `label`); the rest of the hit's excerpt follows as
/// dimmed, but still independently openable, detail lines.
#[derive(Clone)]
pub struct Line {
    pub file: String,
    pub line: usize,
    pub text: String,
    pub anchor: bool,
    pub label: Option<String>,
}

pub struct Node {
    pub seg: String,
    pub path: String,
    pub children: HashMap<String, Node>,
    pub line: Option<Line>,
}

impl Node {
    fn new(seg: &str, path: &str) -> Self {
        Node { seg: seg.to_string(), path: path.to_string(), children: HashMap::new(), line: None }
    }

    /// A node is a "file" (vs. a directory) when its children are line
    /// leaves rather than further path segments.
    pub fn is_file_node(&self) -> bool {
        self.children.values().next().is_some_and(|c| c.line.is_some())
    }
}

/// Weave every line into a Node tree keyed by path segment, with line
/// leaves under their file keyed by a zero-padded line number (plus a
/// tie-breaker index) so insertion order doesn't matter and sort-by-key
/// yields source order.
pub fn build_tree(lines: &[Line]) -> Node {
    let mut root = Node::new("", "");
    for (i, ln) in lines.iter().enumerate() {
        let parts: Vec<&str> = ln.file.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        let mut node = &mut root;
        let mut path = String::new();
        for seg in parts.iter().copied() {
            if !path.is_empty() {
                path.push('/');
            }
            path.push_str(seg);
            node = node
                .children
                .entry(seg.to_string())
                .or_insert_with(|| Node::new(seg, path.as_str()));
        }
        let key = format!("{:020}_{:08}", ln.line, i);
        let leaf_path = format!("{path}\0{key}");
        let leaf = node.children.entry(key.clone()).or_insert_with(|| Node::new(&key, &leaf_path));
        leaf.line = Some(ln.clone());
    }
    root
}

/// One flattened, screen-addressable row: either a folder (directory or
/// file grouping) or a line leaf.
pub struct Row {
    pub id: String,
    pub depth: usize,
    pub is_folder: bool,
    pub seg: String,
    pub is_file_node: bool,
    pub child_count: usize,
    pub expanded: bool,
    /// Sequential jump number across the whole tree, line rows only.
    pub number: Option<usize>,
    pub line: Option<Line>,
}

/// Flatten the tree into display order. A folder is expanded when its path
/// is in `expanded`; everything else stays collapsed.
pub fn build_visible(root: &Node, expanded: &HashSet<String>) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut counter = 0usize;
    visit(root, 0, expanded, &mut rows, &mut counter);
    rows
}

fn visit(node: &Node, depth: usize, expanded: &HashSet<String>, rows: &mut Vec<Row>, counter: &mut usize) {
    let mut keys: Vec<&String> = node.children.keys().collect();
    keys.sort_by_key(|k| k.to_lowercase());
    for key in keys {
        let child = &node.children[key];
        if let Some(line) = &child.line {
            *counter += 1;
            rows.push(Row {
                id: format!("L:{}", child.path),
                depth,
                is_folder: false,
                seg: String::new(),
                is_file_node: false,
                child_count: 0,
                expanded: false,
                number: Some(*counter),
                line: Some(line.clone()),
            });
        } else {
            let is_open = expanded.contains(&child.path);
            rows.push(Row {
                id: format!("F:{}", child.path),
                depth,
                is_folder: true,
                seg: child.seg.clone(),
                is_file_node: child.is_file_node(),
                child_count: child.children.len(),
                expanded: is_open,
                number: None,
                line: None,
            });
            if is_open {
                visit(child, depth + 1, expanded, rows, counter);
            }
        }
    }
}

/// Every directory and file path in the tree, for "start fully expanded".
pub fn all_folder_paths(node: &Node, out: &mut HashSet<String>) {
    for child in node.children.values() {
        if child.line.is_none() {
            out.insert(child.path.clone());
            all_folder_paths(child, out);
        }
    }
}
