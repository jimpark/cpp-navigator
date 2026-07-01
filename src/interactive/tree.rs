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

/// Walk `path` (slash-joined folder segments) from `root` to the node it
/// names, or `None` if no such folder exists in the current tree. Folder
/// children are keyed by their segment, so a folder path resolves segment by
/// segment.
pub fn find_node<'a>(root: &'a Node, path: &str) -> Option<&'a Node> {
    let mut node = root;
    for seg in path.split('/') {
        node = node.children.get(seg)?;
    }
    Some(node)
}

/// Assign sequential jump numbers (1..) to the line rows, left to right.
/// Cheap enough (a field write per row, no allocation) to run over the whole
/// list after a splice rather than tracking a moving offset.
fn renumber(rows: &mut [Row]) {
    let mut n = 0;
    for row in rows.iter_mut() {
        if !row.is_folder {
            n += 1;
            row.number = Some(n);
        }
    }
}

/// Expand the folder row at `index` in place: mark it open and splice its
/// freshly flattened subtree in right after it, instead of rebuilding the
/// entire row list. `expanded` must already contain the folder's path (so
/// nested folders that were previously open re-open too). `root` is the tree
/// the rows were built from.
pub fn splice_expand(rows: &mut Vec<Row>, index: usize, root: &Node, expanded: &HashSet<String>) {
    let path = rows[index].id.trim_start_matches("F:").to_string();
    let depth = rows[index].depth;
    rows[index].expanded = true;
    if let Some(node) = find_node(root, &path) {
        let mut sub = Vec::new();
        let mut counter = 0;
        visit(node, depth + 1, expanded, &mut sub, &mut counter);
        rows.splice(index + 1..index + 1, sub);
    }
    renumber(rows);
}

/// Collapse the folder row at `index` in place: mark it closed and drop the
/// contiguous run of deeper rows that form its subtree, instead of rebuilding
/// the entire row list.
pub fn splice_collapse(rows: &mut Vec<Row>, index: usize) {
    let depth = rows[index].depth;
    rows[index].expanded = false;
    let mut end = index + 1;
    while end < rows.len() && rows[end].depth > depth {
        end += 1;
    }
    rows.drain(index + 1..end);
    renumber(rows);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A comparable fingerprint of a row, covering every field the browser
    /// reads, so splice output can be asserted byte-identical to a full build.
    fn fingerprint(r: &Row) -> (String, usize, bool, String, bool, usize, bool, Option<usize>, Option<(String, usize)>) {
        (
            r.id.clone(),
            r.depth,
            r.is_folder,
            r.seg.clone(),
            r.is_file_node,
            r.child_count,
            r.expanded,
            r.number,
            r.line.as_ref().map(|l| (l.file.clone(), l.line)),
        )
    }

    fn sample_lines() -> Vec<Line> {
        let mut lines = Vec::new();
        for d in 0..4 {
            for s in 0..3 {
                for f in 0..3 {
                    let file = format!("dir{d}/sub{s}/file_{f}.cpp");
                    for n in 0..4 {
                        lines.push(Line {
                            file: file.clone(),
                            line: n + 1,
                            text: format!("{file}:{n}"),
                            anchor: n == 0,
                            label: (n == 0).then(|| format!("[{file}] hit")),
                        });
                    }
                }
            }
        }
        lines
    }

    /// Tiny deterministic PRNG (xorshift64) so the fuzz is reproducible
    /// without pulling in the `rand` crate.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    #[test]
    fn splice_matches_full_build_under_random_toggles() {
        let root = build_tree(&sample_lines());
        let mut folder_paths = HashSet::new();
        all_folder_paths(&root, &mut folder_paths);

        for seed in 1..=25u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15));
            // Start fully expanded, like the browser does.
            let mut expanded: HashSet<String> = folder_paths.clone();
            let mut rows = build_visible(&root, &expanded);

            for _ in 0..400 {
                // Pick a random folder row to toggle.
                let folder_indices: Vec<usize> =
                    rows.iter().enumerate().filter(|(_, r)| r.is_folder).map(|(i, _)| i).collect();
                if folder_indices.is_empty() {
                    break;
                }
                let idx = folder_indices[(rng.next() as usize) % folder_indices.len()];
                let path = rows[idx].id.trim_start_matches("F:").to_string();
                if rows[idx].expanded {
                    expanded.remove(&path);
                    splice_collapse(&mut rows, idx);
                } else {
                    expanded.insert(path);
                    splice_expand(&mut rows, idx, &root, &expanded);
                }

                // Must match a from-scratch flatten of the same expanded set.
                let reference = build_visible(&root, &expanded);
                assert_eq!(rows.len(), reference.len(), "seed {seed}: length drift");
                for (i, (a, b)) in rows.iter().zip(reference.iter()).enumerate() {
                    assert_eq!(fingerprint(a), fingerprint(b), "seed {seed}: row {i} mismatch");
                }
                // Numbers must be gap-free 1..N.
                let mut expect = 0;
                for r in &rows {
                    if let Some(n) = r.number {
                        expect += 1;
                        assert_eq!(n, expect, "seed {seed}: numbering gap");
                    }
                }
            }
        }
    }
}
