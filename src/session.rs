//! Saved sessions: one file per conversation in the project's `sessions`
//! directory, a small index beside them for listing and search, and a short
//! id to come back by (`rusty -c ID`, `/resume ID`).
//!
//! The index holds each session's title and the user's own words, so
//! `--sessions QUERY` and `/resume QUERY` rank sessions with the same BM25
//! index the memory advisor uses, without opening every session file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::context::NOTE;

const INDEX: &str = "index.json";
/// How much of what the user said the index keeps for search.
const TEXT_CHARS: usize = 4000;
const TITLE_CHARS: usize = 60;
const MATCHES: usize = 20;

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The first thing the user asked, shortened.
    pub title: String,
    pub started: u64,
    pub updated: u64,
    pub messages: usize,
    /// What the user said, for search; never the model's or a tool's output.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
}

/// An id no file in `dir` uses yet: eight hex digits from the clock, the
/// process and a counter, so two sessions started in the same second, or
/// by `/clear` right after launch, never share a file.
pub fn new_id(dir: &Path) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    loop {
        let nanos =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let seed = format!("{nanos}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
        let id = format!("{:08x}", rusty::fnv(&seed) as u32);
        if !path(dir, &id).exists() {
            return id;
        }
    }
}

pub fn path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

/// The id a session file is saved under.
pub fn id_of(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

/// What the user typed, in order: plain user turns, not rusty's own notes,
/// summaries or tool results.
fn spoken(history: &[Value]) -> impl Iterator<Item = &str> {
    history
        .iter()
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty() && !c.starts_with(NOTE) && !c.starts_with('['))
}

/// A title from the first thing the user asked: its first line, shortened.
pub fn title_for(history: &[Value]) -> String {
    let Some(first) = spoken(history).next() else { return String::new() };
    let line = first.lines().find(|l| !l.trim().is_empty()).unwrap_or(first).trim();
    let words: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.chars().count() <= TITLE_CHARS {
        return words;
    }
    let cut: String = words.chars().take(TITLE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The user's words for the search index, newest last, within `TEXT_CHARS`.
pub fn text_for(history: &[Value]) -> String {
    let mut text = spoken(history).collect::<Vec<_>>().join("\n");
    if text.chars().count() > TEXT_CHARS {
        let skip = text.chars().count() - TEXT_CHARS;
        text = text.chars().skip(skip).collect();
    }
    text
}

fn read_index(dir: &Path) -> Vec<Entry> {
    std::fs::read_to_string(dir.join(INDEX)).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Records a session in the index, keeping its start time and an earlier
/// title when the new one is empty (a compacted history has no first ask).
pub fn note(dir: &Path, mut entry: Entry) -> anyhow::Result<()> {
    let mut index = read_index(dir);
    if let Some(old) = index.iter().find(|e| e.id == entry.id) {
        if old.started > 0 {
            entry.started = old.started;
        }
        if entry.title.is_empty() {
            entry.title = old.title.clone();
        }
        if entry.text.is_empty() {
            entry.text = old.text.clone();
        }
    }
    if entry.started == 0 {
        entry.started = entry.updated;
    }
    index.retain(|e| e.id != entry.id);
    index.push(entry);
    index.sort_by_key(|e| std::cmp::Reverse(e.updated));
    std::fs::create_dir_all(dir)?;
    rusty::privacy::write_json(&dir.join(INDEX), serde_json::to_value(&index)?)
}

/// Every session in `dir`, newest first. Files the index never saw (from
/// before it existed, or written by hand) are listed by their file time.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut entries = read_index(dir);
    entries.retain(|e| path(dir, &e.id).exists());
    let known: std::collections::HashSet<String> = entries.iter().map(|e| e.id.clone()).collect();
    if let Ok(files) = std::fs::read_dir(dir) {
        for p in files.filter_map(|e| e.ok().map(|e| e.path())) {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".json") || name == INDEX || name.ends_with(".trace.json") {
                continue;
            }
            let id = id_of(&p);
            if known.contains(&id) {
                continue;
            }
            let updated = p
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            entries.push(Entry { id, started: updated, updated, ..Entry::default() });
        }
    }
    entries.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
    entries
}

/// The session with this id, or the one session whose id starts with it.
pub fn find(dir: &Path, key: &str) -> Option<Entry> {
    let all = list(dir);
    if let Some(e) = all.iter().find(|e| e.id == key) {
        return Some(e.clone());
    }
    let mut starts = all.iter().filter(|e| e.id.starts_with(key));
    match (starts.next(), starts.next()) {
        (Some(e), None) => Some(e.clone()),
        _ => None,
    }
}

/// Sessions ranked against `query` by BM25 over their title and the
/// user's words; the best `MATCHES`, newest first among equals.
pub fn search(dir: &Path, query: &str) -> Vec<Entry> {
    let all = list(dir);
    let docs: Vec<Vec<String>> =
        all.iter().map(|e| rusty::recall::terms(&format!("{}\n{}", e.title, e.text))).collect();
    let q = rusty::recall::query(query);
    let scores = rusty::recall::Index::new(&docs).scores(&q);
    let mut hits: Vec<(f64, Entry)> =
        all.into_iter().zip(scores).filter(|(_, s)| *s > 0.0).map(|(e, s)| (s, e)).collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.updated.cmp(&a.1.updated)));
    hits.into_iter().take(MATCHES).map(|(_, e)| e).collect()
}

/// `3m`, `2h`, `5d`.
pub fn age(from: u64, now: u64) -> String {
    let s = now.saturating_sub(from);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// One line per session: id, age, messages, title.
pub fn render(entries: &[Entry], now: u64) -> String {
    if entries.is_empty() {
        return format!("  {}\n", crate::ui::dim("no sessions here yet"));
    }
    let room = crate::ui::width().saturating_sub(28).max(20);
    let mut out = String::new();
    for e in entries {
        let title = if e.title.is_empty() { "(untitled)".to_string() } else { e.title.clone() };
        out.push_str(&format!(
            "  {}  {:>4}  {:>4} msgs  {}\n",
            crate::ui::info(&e.id),
            age(e.updated, now),
            e.messages,
            crate::ui::truncate(&title, room)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rusty-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ids_are_short_and_never_reused() {
        let dir = temp("ids");
        let a = new_id(&dir);
        std::fs::write(path(&dir, &a), "{}").unwrap();
        let b = new_id(&dir);
        assert_eq!(a.len(), 8);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_title_is_the_users_first_ask_and_notes_do_not_count() {
        let history = vec![
            json!({"role": "user", "content": format!("{NOTE}the project has AGENTS.md")}),
            json!({"role": "user", "content": "  make   the parser faster\nand add tests"}),
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": "now the lexer"}),
        ];
        assert_eq!(title_for(&history), "make the parser faster");
        assert_eq!(text_for(&history), "make   the parser faster\nand add tests\nnow the lexer");
        let long = vec![json!({"role": "user", "content": "x".repeat(200)})];
        assert_eq!(title_for(&long).chars().count(), TITLE_CHARS);
        assert!(title_for(&[json!({"role": "user", "content": "[Summary of the conversation so far]"})]).is_empty());
    }

    #[test]
    fn sessions_are_listed_newest_first_found_by_prefix_and_searched() {
        let dir = temp("index");
        for (id, title, text, t) in [
            ("aaaa0001", "fix the login timeout", "fix the login timeout\nthe token expires early", 100),
            ("bbbb0002", "write release notes", "write release notes for 1.4", 300),
            ("cccc0003", "", "", 200),
        ] {
            std::fs::write(path(&dir, id), "{}").unwrap();
            note(
                &dir,
                Entry { id: id.into(), title: title.into(), text: text.into(), updated: t, messages: 2, started: 0 },
            )
            .unwrap();
        }
        // A later save keeps the first title and start time.
        note(&dir, Entry { id: "aaaa0001".into(), updated: 400, messages: 9, ..Entry::default() }).unwrap();
        let all = list(&dir);
        assert_eq!(all.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["aaaa0001", "bbbb0002", "cccc0003"]);
        assert_eq!((all[0].title.as_str(), all[0].started, all[0].messages), ("fix the login timeout", 100, 9));
        assert_eq!(find(&dir, "bbbb").map(|e| e.id), Some("bbbb0002".into()));
        assert!(find(&dir, "zzz").is_none());
        let hits = search(&dir, "token expiry");
        assert_eq!(hits.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["aaaa0001"]);
        assert!(search(&dir, "kubernetes").is_empty());
        let shown = render(&all, 1000);
        assert!(shown.contains("aaaa0001") && shown.contains("(untitled)") && shown.contains("10m"), "{shown}");
        assert_eq!(age(0, 90_000), "1d");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
