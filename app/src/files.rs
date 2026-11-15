//! The workspace walk behind `@file` completion (terminal host only; the web
//! host has no workspace). The walk runs on its own thread the first time an
//! `@` token is typed, under a file cap and a time budget, so a huge tree
//! cannot stall the render loop. It follows the old TUI's rules
//! (crates/modron-tui `collect_files`): the `ignore` crate's walker (.gitignore
//! respected without a git directory, hidden entries skipped), `target` and
//! `node_modules` never entered below the root, the first [`FILE_CAP`] paths
//! found, then sorted.

use std::path::Path;
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Paths the walk keeps (the old TUI's cap).
pub const FILE_CAP: usize = 200;
/// How long the walk may run before it keeps what it has.
pub const WALK_BUDGET: Duration = Duration::from_millis(400);

/// The cached index: the walk's result once it arrives.
struct Index {
    rx: Option<mpsc::Receiver<Vec<String>>>,
    files: Vec<String>,
}

static INDEX: Mutex<Option<Index>> = Mutex::new(None);

/// Walks `root`: relative, `/`-separated paths, at most `cap`, stopping early
/// once `budget` is spent. Sorted.
pub fn walk(root: &Path, cap: usize, budget: Duration) -> Vec<String> {
    let deadline = Instant::now() + budget;
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(entry.file_name().to_str(), Some("target" | "node_modules"))
        })
        .build();
    let mut out = Vec::new();
    for entry in walker.flatten() {
        if out.len() >= cap || Instant::now() >= deadline {
            break;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        out.push(rel.to_string_lossy().replace('\\', "/"));
    }
    out.sort();
    out
}

/// The paths whose text contains `fragment`, ignoring case (the old TUI's
/// rule: a substring, friendlier than a prefix for paths).
pub fn filter(files: &[String], fragment: &str) -> Vec<String> {
    let needle = fragment.to_lowercase();
    files.iter().filter(|path| path.to_lowercase().contains(&needle)).cloned().collect()
}

/// The `@file` candidates for `fragment` from the working directory's index.
/// The first call starts the walk; until it finishes the list is empty, and
/// the next keystroke shows the result.
pub fn matches(fragment: &str) -> Vec<String> {
    let Ok(mut slot) = INDEX.lock() else {
        return Vec::new();
    };
    let index = slot.get_or_insert_with(|| {
        let (tx, rx) = mpsc::channel();
        let root = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let started = std::thread::Builder::new().name("modron-files".into()).spawn(move || {
            let _ = tx.send(walk(&root, FILE_CAP, WALK_BUDGET));
        });
        // A failed spawn drops the sender: the index stays empty.
        let _ = started;
        Index { rx: Some(rx), files: Vec::new() }
    });
    if let Some(rx) = index.rx.as_ref() {
        match rx.try_recv() {
            Ok(files) => {
                index.files = files;
                index.rx = None;
            }
            Err(TryRecvError::Disconnected) => index.rx = None,
            Err(TryRecvError::Empty) => {}
        }
    }
    filter(&index.files, fragment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh directory under the system temp dir, unique per test.
    fn fixture(name: &str) -> std::path::PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("modron-files-{name}-{stamp}"));
        for dir in ["src/lib", "build", "target/debug", ".hidden", "node_modules/pkg"] {
            fs::create_dir_all(root.join(dir)).expect("dir");
        }
        for file in [
            "src/main.rs",
            "src/lib/util.rs",
            "README.md",
            "ignored.txt",
            "build/out.o",
            "target/debug/bin",
            ".hidden/secret",
            "node_modules/pkg/index.js",
        ] {
            fs::write(root.join(file), "x").expect("file");
        }
        fs::write(root.join(".gitignore"), "ignored.txt\nbuild/\n").expect("gitignore");
        root
    }

    #[test]
    fn walk_respects_gitignore_and_skips_the_heavy_dirs_and_hidden_entries() {
        let root = fixture("rules");
        let files = walk(&root, FILE_CAP, Duration::from_secs(10));
        for want in ["README.md", "src/main.rs", "src/lib/util.rs"] {
            assert!(files.iter().any(|f| f == want), "{want} missing from {files:?}");
        }
        for not in [
            "ignored.txt",
            "build/out.o",
            "target/debug/bin",
            ".hidden/secret",
            "node_modules/pkg/index.js",
        ] {
            assert!(!files.iter().any(|f| f == not), "{not} should be skipped: {files:?}");
        }
        assert!(files.windows(2).all(|pair| pair[0] <= pair[1]), "sorted");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn walk_stops_at_the_cap() {
        let root = fixture("cap");
        assert_eq!(walk(&root, 2, Duration::from_secs(10)).len(), 2);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn filter_is_a_case_insensitive_substring() {
        let files = vec!["src/Main.rs".to_string(), "README.md".to_string()];
        assert_eq!(filter(&files, "main"), vec!["src/Main.rs".to_string()]);
        assert_eq!(filter(&files, "").len(), 2);
        assert!(filter(&files, "zzz").is_empty());
    }
}
