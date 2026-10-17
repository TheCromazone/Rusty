//! Session identity, private atomic saves and one writer per transcript.
use anyhow::{bail, Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const INDEX: &str = "index.sqlite";
const INDEX_LOCK: &str = "index.lock";
const PENDING_DIR: &str = ".index-pending";
const MAX_TEXT_CHARS: usize = 4000;
const MAX_TITLE_CHARS: usize = 60;
const MAX_MATCHES: usize = 20;
const MAX_LIST: usize = 50;
const INDEX_VERSION: i64 = 2;

pub fn create(dir: &Path) -> Result<PathBuf> {
    rusty::privacy::private_dir(dir)?;
    for _ in 0..64 {
        let mut bytes = [0; 16];
        SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("session identity unavailable"))?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{id}.json"));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => {
                file.write_all(b"{}")?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not reserve a new session")
}

pub struct Lease(File);
impl Lease {
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::try_acquire(path)?
            .ok_or_else(|| anyhow::anyhow!("session is already open for writing: {}", path.display()))
    }

    fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = rusty::privacy::private_file(&path.with_extension("lock"), true)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Ok(None);
            }
            return Err(error.into());
        }
        Ok(Some(Self(file)))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn save(path: &Path, body: serde_json::Value) -> Result<()> {
    let tmp = path.with_extension("pending");
    let result = (|| {
        rusty::privacy::write_json(&tmp, body)?;
        File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, path).context("publishing session")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Publish a parent transcript only after recording a durable catalog-recovery
/// marker. A crash between transcript rename and SQLite commit can then be
/// repaired on the next catalog open without scanning the transcript directory.
pub fn publish_and_record(
    dir: &Path,
    transcript: &Path,
    body: serde_json::Value,
    history: &[serde_json::Value],
    archived: &[serde_json::Value],
) -> Result<()> {
    let marker = pending_marker(dir, transcript)?;
    save(transcript, body)?;
    publish_marker(&marker)?;
    record(dir, transcript, history, archived)?;
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("clearing session index recovery marker"),
    }
    Ok(())
}

fn pending_marker(dir: &Path, transcript: &Path) -> Result<PathBuf> {
    rusty::privacy::private_dir(&dir.join(PENDING_DIR))?;
    let marker = dir.join(PENDING_DIR).join(id_of(transcript));
    let file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&marker)?;
    file.sync_all()?;
    File::open(dir.join(PENDING_DIR))?.sync_all()?;
    Ok(marker)
}

fn publish_marker(marker: &Path) -> Result<()> {
    let mut file = match OpenOptions::new().write(true).truncate(true).open(marker) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    file.write_all(b"published")?;
    file.sync_all()?;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub started: u64,
    pub updated: u64,
    pub messages: usize,
}

pub fn id_of(path: &Path) -> String {
    path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string()
}

pub fn path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn spoken<'a>(
    history: impl DoubleEndedIterator<Item = &'a serde_json::Value>,
) -> impl DoubleEndedIterator<Item = &'a str> {
    history
        .filter(|m| m["role"] == "user")
        .filter_map(|m| m["content"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with(crate::context::NOTE) && !s.starts_with('['))
}

fn title_for(archived: &[serde_json::Value], history: &[serde_json::Value]) -> String {
    let Some(first) = spoken(archived.iter().chain(history.iter())).next() else { return String::new() };
    let first = rusty::privacy::redact(first).0;
    let line = first.lines().find(|line| !line.trim().is_empty()).unwrap_or(&first).trim();
    let normalized = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_TITLE_CHARS {
        normalized
    } else {
        format!("{}…", normalized.chars().take(MAX_TITLE_CHARS - 1).collect::<String>().trim_end())
    }
}

fn text_for(archived: &[serde_json::Value], history: &[serde_json::Value]) -> String {
    // Redact complete messages before clipping; a clipped secret can lose its
    // identifying key. Walk backwards and stop as soon as the bounded tail is
    // full, instead of reprocessing the entire archived conversation per save.
    let mut pieces = Vec::new();
    let mut remaining = MAX_TEXT_CHARS;
    for raw in spoken(archived.iter().chain(history.iter())).rev() {
        if !pieces.is_empty() {
            remaining -= 1;
        }
        let safe = rusty::privacy::redact(raw).0;
        let count = safe.chars().count();
        let tail = safe.chars().skip(count.saturating_sub(remaining)).collect::<String>();
        let kept = count.min(remaining);
        pieces.push(tail);
        remaining -= kept;
        if remaining == 0 {
            break;
        }
    }
    pieces.reverse();
    pieces.join("\n")
}

struct IndexLease(File);
impl IndexLease {
    fn acquire(dir: &Path) -> Result<Self> {
        let file = rusty::privacy::private_file(&dir.join(INDEX_LOCK), true)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::WouldBlock {
                return Err(error.into());
            }
            if std::time::Instant::now() >= deadline {
                bail!("session catalog is busy; saved transcript is intact, retry shortly");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        Ok(Self(file))
    }
}
impl Drop for IndexLease {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

struct SessionIndex {
    // Keep the cross-process lease through every read or transaction below.
    _lease: IndexLease,
    db: Connection,
}

fn remove_index_files(dir: &Path) {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut name = INDEX.to_string();
        name.push_str(suffix);
        let _ = std::fs::remove_file(dir.join(name));
    }
}

fn fresh_connection(dir: &Path) -> Result<Connection> {
    let path = dir.join(INDEX);
    drop(rusty::privacy::private_file(&path, true)?);
    let db = Connection::open(&path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 0 && version != INDEX_VERSION {
        bail!("unsupported session index version {version}");
    }
    db.execute_batch(
        "PRAGMA journal_mode=DELETE;
         PRAGMA synchronous=FULL;
         CREATE TABLE IF NOT EXISTS sessions(
           id TEXT PRIMARY KEY, title TEXT NOT NULL, started INTEGER NOT NULL,
           updated INTEGER NOT NULL, updated_ns INTEGER NOT NULL, messages INTEGER NOT NULL,
           text TEXT NOT NULL, terms TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS sessions_updated ON sessions(updated_ns DESC, id DESC);
         CREATE VIRTUAL TABLE IF NOT EXISTS session_terms USING fts5(id UNINDEXED, terms, tokenize='unicode61');
         CREATE TABLE IF NOT EXISTS session_index_meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
         PRAGMA user_version=2;",
    )?;
    db.prepare("SELECT id,title,started,updated,updated_ns,messages,text,terms FROM sessions LIMIT 0")?;
    db.prepare("SELECT id,terms FROM session_terms LIMIT 0")?;
    Ok(db)
}

fn index_ready(dir: &Path) -> Result<SessionIndex> {
    rusty::privacy::private_dir(dir)?;
    let lease = IndexLease::acquire(dir)?;
    let mut db = match fresh_connection(dir) {
        Ok(db) => db,
        Err(first) => {
            remove_index_files(dir);
            fresh_connection(dir).with_context(|| format!("rebuilding session index after: {first}"))?
        }
    };
    backfill(&mut db, dir)?;
    recover_pending(&mut db, dir)?;
    Ok(SessionIndex { _lease: lease, db })
}

fn corruption(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<rusqlite::Error>().is_some_and(|error| match error {
            rusqlite::Error::SqliteFailure(code, _) => {
                matches!(code.code, rusqlite::ffi::ErrorCode::DatabaseCorrupt | rusqlite::ffi::ErrorCode::NotADatabase)
            }
            _ => false,
        })
    })
}

fn with_index<T>(dir: &Path, mut operation: impl FnMut(&mut Connection) -> Result<T>) -> Result<T> {
    let mut index = index_ready(dir)?;
    match operation(&mut index.db) {
        Ok(value) => Ok(value),
        Err(first) if corruption(&first) => {
            let SessionIndex { _lease, db } = index;
            drop(db);
            remove_index_files(dir);
            let mut db = fresh_connection(dir).context("recreating corrupt session index")?;
            backfill(&mut db, dir)?;
            recover_pending(&mut db, dir)?;
            operation(&mut db).context("retrying query after session index rebuild")
        }
        Err(error) => Err(error),
    }
}

fn recover_pending(db: &mut Connection, dir: &Path) -> Result<()> {
    let marker_dir = dir.join(PENDING_DIR);
    let Ok(markers) = std::fs::read_dir(&marker_dir) else { return Ok(()) };
    for marker in markers.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
        let id = marker.file_name().and_then(|name| name.to_str()).unwrap_or("");
        if id.is_empty() || id.contains('/') || id.contains('\\') {
            let _ = std::fs::remove_file(marker);
            continue;
        }
        let transcript = dir.join(format!("{id}.json"));
        // Exclude writers through both the SQLite commit and marker removal.
        // A prior lock check followed by a lease release lets a save race this
        // read and lose the marker for a newer transcript version.
        let Some(_writer_lease) = Lease::try_acquire(&transcript)? else { continue };
        let recovered = (|| -> Result<Option<(Entry, u64, String)>> {
            let raw = match std::fs::read_to_string(&transcript) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            };
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            if !value["side"].is_null() {
                return Ok(None);
            }
            let Some(history) = value["history"].as_array() else { return Ok(None) };
            let updated_ns = transcript
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos().min(u64::MAX as u128) as u64);
            let updated = updated_ns / 1_000_000_000;
            let started = value["started"].as_u64().unwrap_or(updated);
            let text = text_for(&[], history);
            Ok(Some((
                Entry { id: id.to_owned(), title: title_for(&[], history), started, updated, messages: history.len() },
                updated_ns,
                text,
            )))
        })()?;
        if let Some((entry, updated_ns, text)) = recovered {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            upsert(&tx, &entry, updated_ns, &text)?;
            tx.commit()?;
        }
        // The writer lease guarantees no concurrent publisher can need this
        // marker. Valid rows are removed only after the SQLite commit.
        std::fs::remove_file(marker)?;
    }
    Ok(())
}

fn backfill(db: &mut Connection, dir: &Path) -> Result<()> {
    let done: bool = db
        .query_row("SELECT value FROM session_index_meta WHERE key='backfill'", [], |row| row.get::<_, i64>(0))
        .optional()?
        .is_some_and(|value| value == 1);
    if done {
        return Ok(());
    }
    db.execute_batch("CREATE TABLE IF NOT EXISTS session_index_meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);")?;
    let mut rows = Vec::new();
    if let Ok(files) = std::fs::read_dir(dir) {
        for path in files.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".json") || name.ends_with(".trace.json") || name.ends_with(".changes.json") {
                continue;
            }
            let Ok(value) = std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|source| serde_json::from_str::<serde_json::Value>(&source).map_err(anyhow::Error::from))
            else {
                continue;
            };
            // Side transcripts live in their own subdirectory. Also exclude
            // any side record copied into this directory by an old version.
            if !value["side"].is_null() {
                continue;
            }
            let Some(history) = value["history"].as_array() else { continue };
            let id = id_of(&path);
            if id.is_empty() || id == "index" {
                continue;
            }
            let updated_ns = path
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos().min(u64::MAX as u128) as u64);
            let updated = updated_ns / 1_000_000_000;
            let started = value["started"].as_u64().unwrap_or(updated);
            let text = text_for(&[], history);
            rows.push((
                Entry { id, title: title_for(&[], history), started, updated, messages: history.len() },
                updated_ns,
                text,
            ));
        }
    }
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (entry, updated_ns, text) in rows {
        upsert(&tx, &entry, updated_ns, &text)?;
    }
    tx.execute("INSERT OR REPLACE INTO session_index_meta(key,value) VALUES('backfill',1)", [])?;
    tx.commit()?;
    Ok(())
}

fn upsert(tx: &rusqlite::Transaction<'_>, entry: &Entry, updated_ns: u64, text: &str) -> Result<()> {
    let terms = rusty::recall::terms(&format!("{}\n{text}", entry.title)).join(" ");
    tx.execute(
        "INSERT INTO sessions(id,title,started,updated,updated_ns,messages,text,terms) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(id) DO UPDATE SET title=excluded.title,started=excluded.started,
           updated=excluded.updated,updated_ns=excluded.updated_ns,messages=excluded.messages,text=excluded.text,terms=excluded.terms",
        params![entry.id, entry.title, entry.started, entry.updated, updated_ns, entry.messages, text, terms],
    )?;
    let rowid: i64 = tx.query_row("SELECT rowid FROM sessions WHERE id=?1", [&entry.id], |row| row.get(0))?;
    tx.execute("DELETE FROM session_terms WHERE rowid=?1", [rowid])?;
    tx.execute("INSERT INTO session_terms(rowid,id,terms) VALUES(?1,?2,?3)", params![rowid, entry.id, terms])?;
    Ok(())
}

pub fn record(
    dir: &Path,
    transcript: &Path,
    history: &[serde_json::Value],
    archived: &[serde_json::Value],
) -> Result<()> {
    let id = id_of(transcript);
    let text = text_for(archived, history);
    let title = title_for(archived, history);
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let mut index = index_ready(dir)?;
    let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let last_updated_ns: i64 = tx
        .query_row("SELECT updated_ns FROM sessions ORDER BY updated_ns DESC LIMIT 1", [], |row| row.get(0))
        .optional()?
        .unwrap_or(0);
    // Transactions serialize every catalog writer, so ties in the wall clock
    // still retain the order in which saves were committed.
    let updated_ns = now_ns.max((last_updated_ns.max(0) as u64).saturating_add(1));
    let now = updated_ns / 1_000_000_000;
    let previous: Option<(u64, String, String)> = tx
        .query_row("SELECT started,title,text FROM sessions WHERE id=?1", [&id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    let (started, title, text) = match previous {
        Some((started, old_title, old_text)) => {
            (started, if title.is_empty() { old_title } else { title }, if text.is_empty() { old_text } else { text })
        }
        None => (now, title, text),
    };
    let entry = Entry { id, title, started, updated: now, messages: archived.len() + history.len() };
    upsert(&tx, &entry, updated_ns, &text)?;
    tx.commit()?;
    Ok(())
}

fn row_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry { id: row.get(0)?, title: row.get(1)?, started: row.get(2)?, updated: row.get(3)?, messages: row.get(4)? })
}

pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    with_index(dir, |db| {
        let mut query = db.prepare(
            "SELECT id,title,started,updated,messages FROM sessions ORDER BY updated_ns DESC,id DESC LIMIT ?1",
        )?;
        let rows = query.query_map([MAX_LIST], row_entry)?;
        let entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries.into_iter().filter(|entry| dir.join(format!("{}.json", entry.id)).is_file()).collect())
    })
}

pub fn latest(dir: &Path) -> Result<Option<Entry>> {
    with_index(dir, |db| {
        let mut query = db.prepare(
            "SELECT id,title,started,updated,messages FROM sessions ORDER BY updated_ns DESC,id DESC LIMIT ?1",
        )?;
        let rows = query.query_map([MAX_LIST], row_entry)?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .find(|entry| dir.join(format!("{}.json", entry.id)).is_file()))
    })
}

pub fn find(dir: &Path, key: &str) -> Result<Option<Entry>> {
    if key.is_empty() {
        return Ok(None);
    }
    with_index(dir, |db| {
        let exact: Option<Entry> = db
            .query_row("SELECT id,title,started,updated,messages FROM sessions WHERE id=?1", [key], row_entry)
            .optional()?;
        if exact.is_some() {
            return Ok(exact);
        }
        let upper = format!("{key}\u{10ffff}");
        let mut stmt = db.prepare(
            "SELECT id,title,started,updated,messages FROM sessions WHERE id>=?1 AND id<?2 ORDER BY id LIMIT 2",
        )?;
        let hits = stmt.query_map(params![key, upper], row_entry)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((hits.len() == 1).then(|| hits[0].clone()).filter(|entry| dir.join(format!("{}.json", entry.id)).is_file()))
    })
}

pub fn search(dir: &Path, query: &str) -> Result<Vec<Entry>> {
    let terms = rusty::recall::terms(query);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let expression =
        terms.iter().map(|term| format!("\"{}\"", term.replace('"', "\"\""))).collect::<Vec<_>>().join(" OR ");
    with_index(dir, |db| {
        let mut stmt = db.prepare(
            "SELECT s.id,s.title,s.started,s.updated,s.messages
             FROM session_terms f JOIN sessions s ON s.rowid=f.rowid
             WHERE session_terms MATCH ?1 ORDER BY bm25(session_terms) LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![expression, MAX_MATCHES], row_entry)?;
        let entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries.into_iter().filter(|entry| dir.join(format!("{}.json", entry.id)).is_file()).collect())
    })
}

pub fn render(entries: &[Entry], now: u64) -> String {
    if entries.is_empty() {
        return format!("  {}\n", crate::ui::dim("no sessions here yet"));
    }
    let room = crate::ui::width().saturating_sub(20).max(10);
    let mut output = String::new();
    for entry in entries {
        let title = if entry.title.is_empty() { "(untitled)" } else { &entry.title };
        output.push_str(&format!(
            "  {}\n    {:>4}  {:>4} msgs  {}\n",
            crate::ui::info(&entry.id),
            age(entry.updated, now),
            entry.messages,
            crate::ui::truncate(title, room)
        ));
    }
    output
}

pub fn age(from: u64, now: u64) -> String {
    let seconds = now.saturating_sub(from);
    match seconds {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86_399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command};

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rusty-session-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        rusty::privacy::private_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn recent_text_keeps_its_bound_without_exposing_clipped_secrets() {
        let current = "x".repeat(MAX_TEXT_CHARS - 1);
        let history = vec![serde_json::json!({"role":"user","content":current})];
        assert_eq!(text_for(&[], &history), current);
        let archived = vec![serde_json::json!({"role":"user","content":"password = INDEX_SECRET_CANARY"})];
        let tail = text_for(&archived, &history);
        assert_eq!(tail.chars().count(), MAX_TEXT_CHARS);
        assert!(!tail.contains("INDEX_SECRET_CANARY"));
        assert_eq!(tail, format!("\n{current}"));
        let unicode = vec![serde_json::json!({"role":"user","content":"界".repeat(MAX_TEXT_CHARS+7)})];
        assert_eq!(text_for(&[], &unicode), "界".repeat(MAX_TEXT_CHARS));
    }

    #[test]
    fn rapid_sessions_preserve_both_transcripts_and_refuse_two_writers() {
        let dir = temp_dir("lease");
        let a = create(&dir).unwrap();
        let b = create(&dir).unwrap();
        assert_ne!(a, b);
        assert_eq!(id_of(&a).len(), 32, "session IDs retain all 128 random bits");
        assert!(id_of(&a).bytes().all(|byte| byte.is_ascii_hexdigit()));
        let lease = Lease::acquire(&a).unwrap();
        save(&a, serde_json::json!({"history":["first"]})).unwrap();
        save(&b, serde_json::json!({"history":["second"]})).unwrap();
        assert!(std::fs::read_to_string(&a).unwrap().contains("first"));
        assert!(std::fs::read_to_string(&b).unwrap().contains("second"));
        assert!(Lease::acquire(&a).is_err());
        drop(lease);
        assert!(Lease::acquire(&a).is_ok());
        assert!(!a.with_extension("pending").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    // Child entry point used by the concurrency test. Each subprocess opens
    // its own SQLite connection and competes for the same index lock.
    #[test]
    fn index_writer_child_entry() {
        let Ok(dir) = std::env::var("RUSTY_TEST_SESSION_INDEX_DIR") else { return };
        let dir = PathBuf::from(dir);
        let transcript = create(&dir).unwrap();
        let id = id_of(&transcript);
        let history = vec![serde_json::json!({"role":"user","content":format!("parallel writer {id}")})];
        save(&transcript, serde_json::json!({"history":history})).unwrap();
        record(&dir, &transcript, &history, &[]).unwrap();
    }

    #[test]
    fn concurrent_process_writers_reopen_and_keep_every_index_row() {
        let dir = temp_dir("writers");
        let exe = std::env::current_exe().unwrap();
        let mut children: Vec<Child> = (0..6)
            .map(|_| {
                Command::new(&exe)
                    .args(["--exact", "session::tests::index_writer_child_entry", "--nocapture"])
                    .env("RUSTY_TEST_SESSION_INDEX_DIR", &dir)
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in &mut children {
            assert!(child.wait().unwrap().success());
        }
        let first = list(&dir).unwrap();
        assert_eq!(first.len(), 6);
        drop(index_ready(&dir).unwrap());
        assert_eq!(list(&dir).unwrap().len(), 6);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_and_corrupt_index_backfill_only_parent_transcripts() {
        let dir = temp_dir("rebuild");
        let legacy = dir.join("1700000000-abcdef.json");
        rusty::privacy::write_json(
            &legacy,
            serde_json::json!({"started": 10, "history":[{"role":"user","content":"legacy searchable amber phrase"}]}),
        )
        .unwrap();
        let sides = dir.join("sides");
        rusty::privacy::private_dir(&sides).unwrap();
        rusty::privacy::write_json(
            &sides.join("side0001.json"),
            serde_json::json!({"side":{"parent":"1700000000-abcdef"},"history":[{"role":"user","content":"secret child turquoise phrase"}]}),
        ).unwrap();

        assert_eq!(search(&dir, "amber").unwrap().len(), 1);
        assert!(search(&dir, "turquoise").unwrap().is_empty());
        remove_index_files(&dir);
        assert_eq!(list(&dir).unwrap().len(), 1);
        assert_eq!(search(&dir, "amber").unwrap().len(), 1);
        std::fs::write(dir.join(INDEX), b"not a sqlite database").unwrap();
        assert_eq!(search(&dir, "amber").unwrap().len(), 1);
        assert!(search(&dir, "turquoise").unwrap().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn query_page_corruption_rebuilds_once_and_retries() {
        use std::io::{Seek, SeekFrom};
        let dir = temp_dir("page-corruption");
        let transcript = create(&dir).unwrap();
        let history = vec![serde_json::json!({"role":"user","content":"rebuildable page phrase"})];
        save(&transcript, serde_json::json!({"history":history})).unwrap();
        record(&dir, &transcript, &history, &[]).unwrap();

        let db = Connection::open(dir.join(INDEX)).unwrap();
        let page_size: u32 = db.pragma_query_value(None, "page_size", |row| row.get(0)).unwrap();
        let root_page: u32 =
            db.query_row("SELECT rootpage FROM sqlite_master WHERE name='sessions'", [], |row| row.get(0)).unwrap();
        drop(db);
        let mut file = OpenOptions::new().write(true).open(dir.join(INDEX)).unwrap();
        let offset = u64::from(root_page.saturating_sub(1)) * u64::from(page_size);
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[0]).unwrap();
        file.sync_all().unwrap();

        let recovered = search(&dir, "rebuildable").unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].id, id_of(&transcript));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pending_marker_recovers_publish_window_without_directory_backfill() {
        let dir = temp_dir("pending-recovery");
        // Mark the already-published transcript as if the process crashed
        // before its SQLite transaction. Normal index backfill is complete,
        // so recovery must use the marker rather than scan all JSON files.
        let old = dir.join("old.json");
        rusty::privacy::write_json(
            &old,
            serde_json::json!({"history":[{"role":"user","content":"old marker phrase"}]}),
        )
        .unwrap();
        drop(index_ready(&dir).unwrap());

        let transcript = create(&dir).unwrap();
        let id = id_of(&transcript);
        let history = vec![serde_json::json!({"role":"user","content":"crash window cobalt phrase"})];
        let marker = pending_marker(&dir, &transcript).unwrap();
        save(&transcript, serde_json::json!({"history":history})).unwrap();
        assert!(marker.exists());

        let found = search(&dir, "cobalt").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, id);
        assert!(!marker.exists());
        assert_eq!(search(&dir, "old").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_clears_stale_and_side_markers_without_indexing_them() {
        let dir = temp_dir("pending-stale");
        let marker_dir = dir.join(PENDING_DIR);
        rusty::privacy::private_dir(&marker_dir).unwrap();
        let stale = marker_dir.join("missing-session");
        std::fs::write(&stale, b"published").unwrap();
        let in_progress = marker_dir.join("writer-in-progress");
        std::fs::write(&in_progress, b"pending").unwrap();
        let live_path = dir.join("writer-in-progress.json");
        let live_lease = Lease::acquire(&live_path).unwrap();
        let side = dir.join("side-session.json");
        rusty::privacy::write_json(
            &side,
            serde_json::json!({
                "side":{"parent":"parent"},
                "history":[{"role":"user","content":"side marker hidden phrase"}]
            }),
        )
        .unwrap();
        let side_marker = marker_dir.join("side-session");
        std::fs::write(&side_marker, b"published").unwrap();

        assert!(search(&dir, "hidden").unwrap().is_empty());
        assert!(!stale.exists() && !side_marker.exists());
        assert!(in_progress.exists(), "a pending marker may belong to a live writer");
        drop(live_lease);
        assert_eq!(list(&dir).unwrap().len(), 0);
        assert!(!in_progress.exists(), "orphan markers are cleared after the writer exits");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn active_writer_keeps_old_transcript_marker_until_new_version_is_recoverable() {
        let dir = temp_dir("live-writer-recovery");
        let transcript = create(&dir).unwrap();
        let id = id_of(&transcript);
        let old = vec![serde_json::json!({"role":"user","content":"old indexed phrase"})];
        save(&transcript, serde_json::json!({"history":old})).unwrap();
        record(&dir, &transcript, &old, &[]).unwrap();

        // Model the save sequence while it still holds its per-transcript
        // lease: marker first, but the old JSON remains visible until rename.
        let writer = Lease::acquire(&transcript).unwrap();
        let marker = pending_marker(&dir, &transcript).unwrap();
        assert_eq!(search(&dir, "old").unwrap()[0].id, id);
        assert!(marker.exists(), "recovery must skip an active writer entirely");

        let new = vec![serde_json::json!({"role":"user","content":"new crash-recovered phrase"})];
        save(&transcript, serde_json::json!({"history":new})).unwrap();
        publish_marker(&marker).unwrap();
        let _ = list(&dir).unwrap();
        assert!(marker.exists(), "recovery must also skip an active writer after publication");
        drop(writer); // Crash before record() commits the new catalog row.

        let found = search(&dir, "crash-recovered").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, id);
        assert!(search(&dir, "old").unwrap().is_empty());
        assert!(!marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recovery_keeps_old_file_mtime_below_newer_indexed_session() {
        let dir = temp_dir("recovery-order");
        drop(index_ready(&dir).unwrap()); // Mark initial backfill complete.
        let older = dir.join("older-pending.json");
        rusty::privacy::write_json(
            &older,
            serde_json::json!({"started":1,"history":[{"role":"user","content":"older recovery row"}]}),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = dir.join("newer-indexed.json");
        rusty::privacy::write_json(
            &newer,
            serde_json::json!({"started":1,"history":[{"role":"user","content":"newer existing row"}]}),
        )
        .unwrap();
        let mtime_ns = |path: &Path| {
            path.metadata().unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
                as u64
        };
        let older_ns = mtime_ns(&older);
        let newer_ns = mtime_ns(&newer);
        assert!(older_ns < newer_ns);

        let mut index = index_ready(&dir).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        let entry = Entry {
            id: id_of(&newer),
            title: "newer existing row".into(),
            started: 1,
            updated: newer_ns / 1_000_000_000,
            messages: 1,
        };
        upsert(&tx, &entry, newer_ns, "newer existing row").unwrap();
        tx.commit().unwrap();
        drop(index);
        let marker = pending_marker(&dir, &older).unwrap();
        publish_marker(&marker).unwrap();

        let newest = latest(&dir).unwrap().unwrap();
        assert_eq!(newest.id, id_of(&newer));
        let index = index_ready(&dir).unwrap();
        let older_indexed_ns: u64 = index
            .db
            .query_row("SELECT updated_ns FROM sessions WHERE id='older-pending'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(older_indexed_ns, older_ns, "recovery preserves the transcript's actual mtime");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rendered_session_id_gets_its_own_full_width_line() {
        let entry = Entry {
            id: "0123456789abcdef0123456789abcdef".into(),
            title: "A useful session".into(),
            updated: 100,
            started: 90,
            messages: 12,
        };
        let rendered = render(&[entry], 120);
        assert!(rendered.starts_with("  0123456789abcdef0123456789abcdef\n"));
        assert!(rendered.contains("    now    12 msgs  A useful session\n"));
    }

    #[test]
    fn large_catalog_search_uses_fts_and_caps_results() {
        let dir = temp_dir("large");
        let mut index = index_ready(&dir).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        for n in 0..5_000 {
            let id = format!("s{n:05}");
            rusty::privacy::write_json(&dir.join(format!("{id}.json")), serde_json::json!({})).unwrap();
            let text = if n == 4_999 { "needle_unique_token searchable catalog" } else { "ordinary catalog entry" };
            let entry = Entry { id, title: text.to_owned(), started: 1, updated: n, messages: 2 };
            upsert(&tx, &entry, n * 1_000_000_000, text).unwrap();
        }
        tx.commit().unwrap();
        drop(index);

        let start = std::time::Instant::now();
        let found = search(&dir, "needle_unique_token").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, "s04999");
        assert!(start.elapsed() < std::time::Duration::from_secs(3));
        assert!(search(&dir, "catalog").unwrap().len() <= MAX_MATCHES);
        assert_eq!(list(&dir).unwrap().len(), MAX_LIST);

        let index = index_ready(&dir).unwrap();
        let plan: String = index
            .db
            .query_row(
                "EXPLAIN QUERY PLAN SELECT rowid FROM session_terms WHERE session_terms MATCH 'needle'",
                [],
                |row| row.get(3),
            )
            .unwrap();
        assert!(plan.contains("VIRTUAL TABLE INDEX"), "query plan: {plan}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn newest_session_uses_subsecond_update_order() {
        let dir = temp_dir("same-second");
        let mut index = index_ready(&dir).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        for (id, updated_ns) in [("older", 1_800_000_000_100_000_000), ("newer", 1_800_000_000_900_000_000)] {
            rusty::privacy::write_json(&dir.join(format!("{id}.json")), serde_json::json!({})).unwrap();
            let entry = Entry { id: id.into(), title: id.into(), started: 10, updated: 1_800_000_000, messages: 2 };
            upsert(&tx, &entry, updated_ns, id).unwrap();
        }
        tx.commit().unwrap();
        drop(index);

        let newest = latest(&dir).unwrap().unwrap();
        assert_eq!(newest.id, "newer");
        assert_eq!(newest.updated, 1_800_000_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn browser_index_redacts_secret_values_but_keeps_source_expressions() {
        let dir = temp_dir("private-index");
        let path = create(&dir).unwrap();
        let id = id_of(&path);
        let secret = "CanarySecret-Index-9274";
        let history = vec![serde_json::json!({
            "role":"user",
            "content":format!("let token = self.token.clone();\npassword={secret}")
        })];
        record(&dir, &path, &history, &[]).unwrap();

        let index = Connection::open(dir.join(INDEX)).unwrap();
        let (title, text, terms): (String, String, String) = index
            .query_row("SELECT title,text,terms FROM sessions WHERE id=?1", [&id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert!(text.contains("let token = self.token.clone();"), "{text}");
        assert!(text.contains("password=[redacted]"), "{text}");
        assert!(!title.contains(secret) && !text.contains(secret) && !terms.contains(secret));
        assert!(search(&dir, secret).unwrap().is_empty());
        assert!(search(&dir, "self.token.clone").unwrap().iter().any(|entry| entry.id == id));
        assert!(!render(&list(&dir).unwrap(), rusty::unix_now()).contains(secret));
        let bytes = std::fs::read(dir.join(INDEX)).unwrap();
        assert!(!bytes.windows(secret.len()).any(|window| window == secret.as_bytes()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
