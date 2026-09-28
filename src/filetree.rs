//! The file tree in the sidebar, like the one in TUIOS: the focused pane's
//! project, folders opened and closed with a click or Enter, files opened in
//! `$EDITOR`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// At most this many rows, so a huge folder does not stall the server.
const MAX_ROWS: usize = 400;

/// How often the shown folders are read again.
const REFRESH: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    /// How deep below the root, 0 for its own entries.
    pub depth: u16,
    pub dir: bool,
    /// For a folder: whether its entries are shown.
    pub open: bool,
    /// Changed since the last commit, set by the sidebar from git.
    pub changed: bool,
}

#[derive(Default)]
pub struct FileTree {
    pub root: PathBuf,
    /// The folders that are open, kept when the root changes and back.
    open: BTreeSet<PathBuf>,
    pub entries: Vec<Entry>,
    read_at: Option<Instant>,
}

impl FileTree {
    /// Shows `root`, reading the folders again when it changed or a moment
    /// has passed.
    pub fn refresh(&mut self, root: &Path) {
        let stale = self.read_at.is_none_or(|at| at.elapsed() >= REFRESH);
        if root != self.root || stale {
            root.clone_into(&mut self.root);
            self.reload();
        }
    }

    /// Opens or closes folder `i`. False if it is no folder.
    pub fn toggle(&mut self, i: usize) -> bool {
        let Some(entry) = self.entries.get(i).filter(|e| e.dir) else {
            return false;
        };
        let path = entry.path.clone();
        if !self.open.remove(&path) {
            self.open.insert(path);
        }
        self.reload();
        true
    }

    /// Where `h` goes from entry `i`: closes it if it is an open folder and
    /// stays, else to the folder it is in. Returns the entry to select.
    pub fn collapse(&mut self, i: usize) -> Option<usize> {
        let entry = self.entries.get(i)?;
        if entry.dir && entry.open {
            self.toggle(i);
            return Some(i);
        }
        let parent = entry.path.parent()?;
        self.entries.iter().position(|e| e.path == parent)
    }

    fn reload(&mut self) {
        let mut entries = Vec::new();
        read(&self.root, 0, &self.open, &mut entries);
        self.entries = entries;
        self.read_at = Some(Instant::now());
    }
}

/// Appends the entries of `dir` to `out`: folders first, then files, by
/// name, hidden ones left out, open folders with their entries.
fn read(dir: &Path, depth: u16, open: &BTreeSet<PathBuf>, out: &mut Vec<Entry>) {
    let Ok(listing) = fs::read_dir(dir) else {
        return;
    };
    let mut found: Vec<(bool, String, PathBuf)> = listing
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            // Follows links, so a linked folder is a folder.
            let dir = e.path().is_dir();
            Some((dir, name, e.path()))
        })
        .collect();
    found.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    for (is_dir, name, path) in found {
        if out.len() >= MAX_ROWS {
            return;
        }
        let is_open = is_dir && open.contains(&path);
        out.push(Entry {
            path: path.clone(),
            name,
            depth,
            dir: is_dir,
            open: is_open,
            changed: false,
        });
        if is_open {
            read(&path, depth + 1, open, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small project in a folder of its own per test, as tests run at
    /// the same time.
    fn tree(test: &str) -> (PathBuf, FileTree) {
        let root = std::env::temp_dir().join(format!("hivemux-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("src/inner")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        for file in [
            "README.md",
            "b.txt",
            "src/main.rs",
            "src/inner/deep.rs",
            ".hidden",
        ] {
            fs::write(root.join(file), "").unwrap();
        }
        let mut tree = FileTree::default();
        tree.refresh(&root);
        (root, tree)
    }

    fn names(tree: &FileTree) -> Vec<String> {
        tree.entries
            .iter()
            .map(|e| format!("{}{}", "  ".repeat(e.depth.into()), e.name))
            .collect()
    }

    #[test]
    fn folders_first_hidden_left_out() {
        let (root, tree) = tree("sorted");
        assert_eq!(names(&tree), ["src", "b.txt", "README.md"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn folders_open_close_and_collapse_to_the_parent() {
        let (root, mut tree) = tree("toggle");
        assert!(tree.toggle(0));
        assert!(tree.toggle(1));
        assert_eq!(
            names(&tree),
            [
                "src",
                "  inner",
                "    deep.rs",
                "  main.rs",
                "b.txt",
                "README.md"
            ]
        );
        // From a file to its folder, then closing that folder.
        assert_eq!(tree.collapse(2), Some(1));
        assert_eq!(tree.collapse(1), Some(1));
        assert_eq!(
            names(&tree),
            ["src", "  inner", "  main.rs", "b.txt", "README.md"]
        );
        assert!(!tree.toggle(4), "a file does not open");
        fs::remove_dir_all(root).unwrap();
    }
}
