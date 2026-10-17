//! What a swarm's workers share while they run: a board of one-line findings.
//! Each worker sees what its peers posted since its last step, so the swarm
//! builds on its own discoveries instead of repeating them.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Most tasks one swarm call runs; the rest are reported as not run.
pub const MAX_JOBS: usize = 32;
const MAX_NOTES: usize = 200;
const NOTE_CHARS: usize = 300;

#[derive(Clone, Default)]
pub struct Board(Arc<Mutex<Vec<Note>>>);

struct Note {
    author: usize,
    label: String,
    text: String,
}

impl Board {
    /// Posts one finding. Duplicates and posts past the cap are dropped.
    pub fn post(&self, author: usize, label: &str, text: &str) -> &'static str {
        let text: String = text.trim().chars().take(NOTE_CHARS).collect();
        if text.is_empty() {
            return "nothing to share";
        }
        let mut notes = self.0.lock().unwrap();
        if notes.iter().any(|n| n.text == text) {
            return "already shared by a worker";
        }
        if notes.len() >= MAX_NOTES {
            return "the board is full; put it in your report";
        }
        notes.push(Note { author, label: label.to_string(), text });
        "shared with the other workers"
    }

    /// Peers' notes posted after `seen`, which then moves to the end.
    pub fn unseen(&self, me: usize, seen: &mut usize) -> Vec<String> {
        let notes = self.0.lock().unwrap();
        let fresh = notes[*seen..].iter().filter(|n| n.author != me).map(|n| format!("[{}] {}", n.label, n.text));
        let fresh = fresh.collect();
        *seen = notes.len();
        fresh
    }

    /// Every note, for the lead.
    pub fn summary(&self) -> String {
        let notes = self.0.lock().unwrap();
        notes.iter().map(|n| format!("- [{}] {}", n.label, n.text)).collect::<Vec<_>>().join("\n")
    }
}

/// Most blind spots named to the lead; the rest are counted.
const MAX_UNOPENED: usize = 30;

/// Files successfully inspected through read_file or outline. Shell arguments
/// are path mentions, not evidence that file contents were observed.
pub fn opened_by(cwd: &Path, tool: &str, args: &serde_json::Value) -> Vec<PathBuf> {
    let words: Vec<&str> = match tool {
        "read_file" | "outline" => args["path"].as_str().into_iter().collect(),
        _ => Vec::new(),
    };
    words
        .into_iter()
        .filter(|w| !w.is_empty() && !w.starts_with('-'))
        .filter_map(|w| cwd.join(w).canonicalize().ok())
        .filter(|p| p.is_file() && p.starts_with(cwd))
        .collect()
}

/// Files beside the ones the workers opened that none of them did: where a
/// swarm's blind spots most likely are. Returns the first few, relative to
/// `cwd`, and how many there are in all.
pub fn unopened_neighbours(cwd: &Path, opened: &BTreeSet<PathBuf>) -> (Vec<String>, usize) {
    let dirs: BTreeSet<&Path> = opened.iter().filter_map(|p| p.parent()).collect();
    let mut missed: Vec<String> = dirs
        .into_iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && !opened.contains(p))
        .filter(|p| !p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with('.') || n.ends_with(".lock")))
        .filter_map(|p| p.strip_prefix(cwd).ok().map(|r| r.display().to_string()))
        .collect();
    missed.sort();
    let total = missed.len();
    missed.truncate(MAX_UNOPENED);
    (missed, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workers_see_only_new_notes_from_peers() {
        let b = Board::default();
        let (mut seen_a, mut seen_b) = (0, 0);
        assert_eq!(b.post(0, "auth", "token check skipped at api.rs:42"), "shared with the other workers");
        assert_eq!(b.post(1, "db", "token check skipped at api.rs:42"), "already shared by a worker");
        b.post(1, "db", "migration 7 drops an index");
        assert_eq!(b.unseen(0, &mut seen_a), vec!["[db] migration 7 drops an index"]);
        assert!(b.unseen(0, &mut seen_a).is_empty());
        assert_eq!(b.unseen(1, &mut seen_b), vec!["[auth] token check skipped at api.rs:42"]);
        assert_eq!(b.summary().lines().count(), 2);
        assert_eq!(b.post(2, "x", "  "), "nothing to share");
    }

    #[test]
    fn blind_spots_are_the_unopened_files_beside_the_opened_ones() {
        let dir = std::env::temp_dir().join(format!("rusty-swarm-cover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("shop")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        for f in ["shop/cart.py", "shop/tax.py", "shop/pricing.py", "shop/.cache", "docs/guide.md"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let cwd = dir.canonicalize().unwrap();
        let mut opened = BTreeSet::new();
        opened.extend(opened_by(&cwd, "read_file", &serde_json::json!({"path": "shop/cart.py"})));
        opened.extend(opened_by(&cwd, "outline", &serde_json::json!({"path": "shop/pricing.py"})));
        assert!(opened_by(&cwd, "bash", &serde_json::json!({"command": "ls shop/tax.py"})).is_empty());
        opened.extend(opened_by(&cwd, "bash", &serde_json::json!({"command": "cat /etc/hostname nosuch.py"})));
        assert_eq!(opened.len(), 2, "{opened:?}");
        // docs/ was never visited, so it is not a blind spot of this swarm.
        assert_eq!(unopened_neighbours(&cwd, &opened), (vec!["shop/tax.py".to_string()], 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn notes_are_bounded() {
        let b = Board::default();
        for i in 0..MAX_NOTES {
            b.post(0, "w", &format!("finding {i}"));
        }
        assert_eq!(b.post(1, "w", "one more"), "the board is full; put it in your report");
        b.post(0, "w", &"x".repeat(1000));
        assert!(b.summary().lines().all(|l| l.chars().count() < NOTE_CHARS + 20));
    }
}
