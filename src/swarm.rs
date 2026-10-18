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

/// Lines in a file, read in pieces rather than whole; a last line without a
/// newline counts.
fn line_count(path: &Path) -> std::io::Result<usize> {
    use std::io::Read;
    let mut file = std::io::BufReader::new(std::fs::File::open(path)?);
    let (mut lines, mut last, mut buf) = (0, b'\n', [0u8; 8192]);
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            return Ok(lines + usize::from(last != b'\n'));
        }
        lines += buf[..n].iter().filter(|&&b| b == b'\n').count();
        last = buf[n - 1];
    }
}

/// Non-code files a report may cite, besides the source extensions.
const TEXT_EXT: &[&str] = &["md", "txt", "json", "toml", "yaml", "yml", "cfg", "ini", "html", "css", "sql", "lock"];

/// Most references one report has checked; the rest are taken on trust.
const MAX_REFERENCES: usize = 100;

/// The `path:line` and `path:start-end` references in a worker's report
/// that point at nothing: a file that isn't there, or lines past its end.
/// A report's references are what the lead reads next, so a wrong one
/// costs a wasted read at best and an invented finding at worst. Returns a
/// note for the lead when any are wrong. Paths outside `cwd` are skipped.
pub fn check_references(cwd: &Path, report: &str) -> Option<String> {
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut seen = BTreeSet::new();
    let mut bad = Vec::new();
    for word in report.split_whitespace() {
        // A leading dot belongs to the path (`./src`); a trailing one ends a sentence.
        let word = word.trim_start_matches(|c: char| "`'\"([<*".contains(c));
        let word = word.trim_end_matches(|c: char| "`'\")]>,;.:*".contains(c));
        let Some((path, lines)) = word.split_once(':') else { continue };
        let (start, end) = match lines.split_once('-') {
            Some((a, b)) => (a.parse::<usize>(), b.parse::<usize>()),
            None => (lines.parse::<usize>(), lines.parse::<usize>()),
        };
        let (Ok(start), Ok(end)) = (start, end) else { continue };
        let full = cwd.join(path);
        // Resolved, so a symlink inside the workspace can't lead the check out of it.
        let real = full.canonicalize().ok();
        let outside = !full.starts_with(cwd)
            || full.components().any(|c| c == std::path::Component::ParentDir)
            || real.as_ref().is_some_and(|r| !r.starts_with(&root));
        // A bare name that isn't here and doesn't end like a file is more
        // likely a host (`api.example.com:443`) than a wrong reference.
        let ext = path.rsplit('.').next().unwrap_or("");
        let file_like = crate::tools::SOURCE_EXT.contains(&ext) || TEXT_EXT.contains(&ext);
        let host_like = !path.contains('/') && !full.is_file() && !file_like;
        if !path.contains('.') || path.contains("://") || outside || host_like || !seen.insert(word.to_string()) {
            continue;
        }
        if seen.len() > MAX_REFERENCES {
            break;
        }
        if start == 0 || start > end {
            bad.push(format!("{word} (not a line range)"));
            continue;
        }
        match real.as_deref().map(line_count) {
            Some(Ok(n)) if end > n => bad.push(format!("{word} ({path} has {n} lines)")),
            Some(Ok(_)) => {}
            _ => bad.push(format!("{word} (no such file)")),
        }
    }
    (!bad.is_empty()).then(|| {
        format!(
            "Reference check: {} of the {} path:line references in this report point at nothing: {}. Treat what rests on them as unconfirmed.",
            bad.len(),
            seen.len().min(MAX_REFERENCES),
            bad.join(", ")
        )
    })
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

    #[test]
    fn references_past_the_end_or_to_missing_files_are_named() {
        let dir = std::env::temp_dir().join(format!("rusty-swarm-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/cache.py"), "a\nb\nc\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "one line, no newline").unwrap();
        let fine = "References:\n- `src/cache.py:2-3` writes it\n- ./src/cache.py:1, notes.txt:1. At 10:30 see https://x.io:443\n\
                    The API is at api.example.com:443, the cache at 127.0.0.1:6379.";
        assert_eq!(check_references(&dir, fine), None);
        let wrong =
            "- src/cache.py:2-9 (writes)\n- src/gone.py:4\n- src/cache.py:3-2\n- src/cache.py:0\n- ../outside.py:1";
        let note = check_references(&dir, wrong).unwrap();
        assert!(note.starts_with("Reference check: 4 of the 4 "), "{note}");
        for want in [
            "src/cache.py:2-9 (src/cache.py has 3 lines)",
            "src/gone.py:4 (no such file)",
            "src/cache.py:3-2 (not a line range)",
            "src/cache.py:0 (not",
        ] {
            assert!(note.contains(want), "{want} missing from {note}");
        }
        assert!(!note.contains("outside"), "{note}");
        // A link inside the workspace to a file outside it is not checked.
        let away = std::env::temp_dir().join(format!("rusty-swarm-away-{}.py", std::process::id()));
        std::fs::write(&away, "x\n").unwrap();
        std::os::unix::fs::symlink(&away, dir.join("src/link.py")).unwrap();
        assert_eq!(check_references(&dir, "- src/link.py:9"), None);
        let _ = std::fs::remove_file(&away);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
