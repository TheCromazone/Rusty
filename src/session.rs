//! Session identity, private atomic saves and one writer per transcript.
use anyhow::{bail, Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const INDEX: &str = "index.sqlite";
const INDEX_LOCK: &str = "index.lock";
const PENDING_DIR: &str = ".index-pending";
const MAX_TEXT_CHARS: usize = 4000;
const MAX_TITLE_CHARS: usize = 60;
const MAX_MATCHES: usize = 20;
const MAX_LIST: usize = 50;
const INDEX_VERSION: i64 = 3;

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

/// The one writer of a transcript: an exclusive `flock` on `<id>.lock`.
/// The lock file exists only while it is held, so a project's session
/// directory does not collect one empty file per session ever opened.
pub struct Lease {
    file: File,
    path: PathBuf,
    transcript: PathBuf,
    /// This writer created the transcript's catalog-recovery marker.
    marked: AtomicBool,
    /// The catalog matches the last transcript this writer published.
    indexed: AtomicBool,
}
impl Lease {
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::try_acquire(path)?
            .ok_or_else(|| anyhow::anyhow!("session is already open for writing: {}", path.display()))
    }

    fn try_acquire(transcript: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        let path = transcript.with_extension("lock");
        // A releasing holder unlinks the file before it unlocks. Whoever
        // then locks that orphaned inode must not count as the writer, or a
        // third process could lock a fresh file at the same path as well.
        for _ in 0..64 {
            let file = rusty::privacy::private_file(&path, true)?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(error.into());
            }
            let held = file.metadata()?;
            match std::fs::symlink_metadata(&path) {
                Ok(current) if current.dev() == held.dev() && current.ino() == held.ino() => {
                    return Ok(Some(Self {
                        file,
                        path,
                        transcript: transcript.to_path_buf(),
                        marked: AtomicBool::new(false),
                        indexed: AtomicBool::new(false),
                    }));
                }
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("session lock keeps changing: {}", path.display())
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        // An unindexed publish keeps its marker for the next catalog open.
        if self.marked.load(Ordering::Acquire) && self.indexed.load(Ordering::Acquire) {
            if let Some(dir) = self.transcript.parent() {
                let _ = std::fs::remove_file(dir.join(PENDING_DIR).join(id_of(&self.transcript)));
            }
        }
        // Unlink while still holding the lock; see `try_acquire`.
        let _ = std::fs::remove_file(&self.path);
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn save(path: &Path, mut body: serde_json::Value) -> Result<()> {
    scrub(&mut body);
    let tmp = path.with_extension("pending");
    let result = (|| {
        let json = serde_json::to_vec(&body)?;
        let mut writer = std::io::BufWriter::new(rusty::privacy::private_file(&tmp, false)?);
        if json.len() >= COMPRESS_FROM {
            let mut gzip = flate2::write::GzEncoder::new(&mut writer, flate2::Compression::new(3));
            gzip.write_all(&json)?;
            gzip.finish()?;
        } else {
            writer.write_all(&json)?;
        }
        writer.into_inner().map_err(|error| error.into_error())?.sync_all()?;
        std::fs::rename(&tmp, path).context("publishing session")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

/// Transcripts this large are saved gzip-compressed. Tool output makes up
/// most of a long session and compresses about 3x; small sessions stay
/// plain JSON. Level 3 costs about 2 ms for 200 KB, close to level 6's size.
const COMPRESS_FROM: usize = 64 * 1024;

/// A transcript's JSON text, whether it was saved plain or compressed. The
/// file keeps its `.json` name either way; `gzip -dc` reads a compressed one.
pub fn read(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let bytes = std::fs::read(path)?;
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut text = String::new();
        flate2::read::GzDecoder::new(bytes.as_slice()).read_to_string(&mut text)?;
        return Ok(text);
    }
    String::from_utf8(bytes).map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// How many clean history messages `scrub` remembers (32 bytes each).
const MAX_CLEAN: usize = 1 << 16;

/// `privacy::scrub`, without rescanning history messages it already found
/// clean. Every save repeats the whole history, almost all of it unchanged
/// since the last save, and redaction is a pure function of the text for
/// the life of the process. A message is skipped only when the SHA-256 of
/// its exact JSON is one that scrubbing left unchanged before.
fn scrub(body: &mut serde_json::Value) {
    static CLEAN: std::sync::Mutex<std::collections::BTreeSet<[u8; 32]>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let history = body.get_mut("history").map(serde_json::Value::take);
    rusty::privacy::scrub(body);
    let Some(mut history) = history else { return };
    if let serde_json::Value::Array(items) = &mut history {
        let mut clean = CLEAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if clean.len() > MAX_CLEAN {
            clean.clear();
        }
        for item in items {
            let Ok(raw) = serde_json::to_vec(item) else {
                rusty::privacy::scrub(item);
                continue;
            };
            let mut digest = [0; 32];
            digest.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, &raw).as_ref());
            if clean.contains(&digest) {
                continue;
            }
            rusty::privacy::scrub(item);
            if serde_json::to_vec(item).is_ok_and(|after| after == raw) {
                clean.insert(digest);
            }
        }
    } else {
        rusty::privacy::scrub(&mut history);
    }
    body["history"] = history;
}

/// Publish a parent transcript only once a durable catalog-recovery marker
/// exists. A crash between transcript rename and SQLite commit can then be
/// repaired on the next catalog open without scanning the transcript
/// directory. One marker covers every save the writer makes: recovery skips
/// it while the lease is held, and the lease clears it on release once the
/// last save it published is in the catalog.
pub fn publish_and_record(
    dir: &Path,
    lease: &Lease,
    body: serde_json::Value,
    history: &[serde_json::Value],
    archived: &[serde_json::Value],
) -> Result<()> {
    let transcript = lease.transcript.as_path();
    if !lease.marked.load(Ordering::Acquire) {
        pending_marker(dir, transcript)?;
        lease.marked.store(true, Ordering::Release);
    }
    lease.indexed.store(false, Ordering::Release);
    save(transcript, body)?;
    record(dir, transcript, history, archived)?;
    lease.indexed.store(true, Ordering::Release);
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

/// One row per parent session, plus a contentless full-text index of its
/// terms. Only the index of the terms is kept, never their text: the text
/// is rebuilt from the transcript whenever it changes. The explicit integer
/// key is what the full-text rows point at, so VACUUM cannot renumber it.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS sessions(
       n INTEGER PRIMARY KEY, id TEXT NOT NULL UNIQUE, title TEXT NOT NULL, started INTEGER NOT NULL,
       updated INTEGER NOT NULL, updated_ns INTEGER NOT NULL, messages INTEGER NOT NULL
     );
     CREATE INDEX IF NOT EXISTS sessions_updated ON sessions(updated_ns DESC, id DESC);
     CREATE VIRTUAL TABLE IF NOT EXISTS session_terms
       USING fts5(terms, content='', contentless_delete=1, tokenize='unicode61');";

fn fresh_connection(dir: &Path) -> Result<Connection> {
    let path = dir.join(INDEX);
    drop(rusty::privacy::private_file(&path, true)?);
    let db = Connection::open(&path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 0 && version != 2 && version != INDEX_VERSION {
        bail!("unsupported session index version {version}");
    }
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
    if version == 2 {
        migrate_v2(&db)?;
    }
    db.execute_batch(SCHEMA)?;
    db.pragma_update(None, "user_version", INDEX_VERSION)?;
    db.prepare("SELECT n,id,title,started,updated,updated_ns,messages FROM sessions LIMIT 0")?;
    db.prepare("SELECT rowid FROM session_terms LIMIT 0")?;
    Ok(db)
}

/// Version 2 stored each session's recent text and its terms in the row and
/// a third copy inside the full-text table. Keep every row (a rebuild from
/// the transcripts would lose titles a compaction has since dropped from the
/// history) and its indexed terms, then hand the freed pages back.
fn migrate_v2(db: &Connection) -> Result<()> {
    db.execute_batch(&format!(
        "BEGIN IMMEDIATE;
         ALTER TABLE sessions RENAME TO sessions_v2;
         DROP INDEX IF EXISTS sessions_updated;
         DROP TABLE session_terms;
         {SCHEMA}
         INSERT INTO sessions(n,id,title,started,updated,updated_ns,messages)
           SELECT rowid,id,title,started,updated,updated_ns,messages FROM sessions_v2;
         INSERT INTO session_terms(rowid,terms) SELECT rowid,terms FROM sessions_v2;
         DROP TABLE sessions_v2;
         PRAGMA user_version={INDEX_VERSION};
         COMMIT;
         VACUUM;"
    ))
    .context("upgrading the session index")
}

fn index_ready(dir: &Path, discover_unindexed: bool) -> Result<SessionIndex> {
    rusty::privacy::private_dir(dir)?;
    let lease = IndexLease::acquire(dir)?;
    let mut backfill_needed = discover_unindexed || !dir.join(INDEX).exists();
    let mut db = match fresh_connection(dir) {
        Ok(db) => db,
        Err(first) => {
            remove_index_files(dir);
            backfill_needed = true;
            fresh_connection(dir).with_context(|| format!("rebuilding session index after: {first}"))?
        }
    };
    if backfill_needed {
        backfill(&mut db, dir)?;
    }
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
    let mut index = index_ready(dir, true)?;
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
            let raw = match read(&transcript) {
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
            upsert(&tx, &entry, updated_ns, Some(&text))?;
            tx.commit()?;
        }
        // The writer lease guarantees no concurrent publisher can need this
        // marker. Valid rows are removed only after the SQLite commit.
        std::fs::remove_file(marker)?;
    }
    Ok(())
}

/// Indexes the session files the catalog has no row for: written before it
/// existed, by an older rusty, or copied in. Files it knows are not read, so
/// on most opens this costs one directory listing.
fn backfill(db: &mut Connection, dir: &Path) -> Result<()> {
    let known: std::collections::HashSet<String> =
        db.prepare("SELECT id FROM sessions")?.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    let mut rows = Vec::new();
    if let Ok(files) = std::fs::read_dir(dir) {
        for path in files.filter_map(|entry| entry.ok().map(|entry| entry.path())) {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".json") || name.ends_with(".trace.json") || name.ends_with(".changes.json") {
                continue;
            }
            let id = id_of(&path);
            if id.is_empty() || id == "index" || known.contains(&id) {
                continue;
            }
            let Ok(value) = read(&path)
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
            // A session nothing was said in yet (one just reserved) has no row.
            let Some(history) = value["history"].as_array().filter(|history| !history.is_empty()) else { continue };
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
    if rows.is_empty() {
        return Ok(());
    }
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for (entry, updated_ns, text) in rows {
        upsert(&tx, &entry, updated_ns, Some(&text))?;
    }
    tx.commit()?;
    Ok(())
}

/// Writes a session's row. `text` is what its terms come from; `None` keeps
/// the terms already indexed, for a history with nothing new to say.
fn upsert(tx: &rusqlite::Transaction<'_>, entry: &Entry, updated_ns: u64, text: Option<&str>) -> Result<()> {
    let n: i64 = tx.query_row(
        "INSERT INTO sessions(id,title,started,updated,updated_ns,messages) VALUES(?1,?2,?3,?4,?5,?6)
         ON CONFLICT(id) DO UPDATE SET title=excluded.title,started=excluded.started,
           updated=excluded.updated,updated_ns=excluded.updated_ns,messages=excluded.messages
         RETURNING n",
        params![entry.id, entry.title, entry.started, entry.updated, updated_ns, entry.messages],
        |row| row.get(0),
    )?;
    if let Some(text) = text {
        let terms = rusty::recall::terms(&format!("{}\n{text}", entry.title)).join(" ");
        tx.execute("DELETE FROM session_terms WHERE rowid=?1", [n])?;
        tx.execute("INSERT INTO session_terms(rowid,terms) VALUES(?1,?2)", params![n, terms])?;
    }
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
    // A normal save already has a durable pending marker, so this transaction
    // can update its row directly. Discover copied-in legacy files lazily when
    // the user browses or resumes the catalog, not on every incremental save.
    let mut index = index_ready(dir, false)?;
    let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let last_updated_ns: i64 = tx
        .query_row("SELECT updated_ns FROM sessions ORDER BY updated_ns DESC LIMIT 1", [], |row| row.get(0))
        .optional()?
        .unwrap_or(0);
    // Transactions serialize every catalog writer, so ties in the wall clock
    // still retain the order in which saves were committed.
    let updated_ns = now_ns.max((last_updated_ns.max(0) as u64).saturating_add(1));
    let now = updated_ns / 1_000_000_000;
    let previous: Option<(u64, String)> = tx
        .query_row("SELECT started,title FROM sessions WHERE id=?1", [&id], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    // The title is the session's first ask. A compacted or resumed history
    // no longer starts with it, so the first title recorded stays. A history
    // with nothing said in it keeps the terms already indexed.
    let (started, title, text) = match previous {
        Some((started, old_title)) => {
            (started, if old_title.is_empty() { title } else { old_title }, Some(text).filter(|text| !text.is_empty()))
        }
        None => (now, title, Some(text)),
    };
    let entry = Entry { id, title, started, updated: now, messages: archived.len() + history.len() };
    upsert(&tx, &entry, updated_ns, text.as_deref())?;
    tx.commit()?;
    Ok(())
}

fn row_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry { id: row.get(0)?, title: row.get(1)?, started: row.get(2)?, updated: row.get(3)?, messages: row.get(4)? })
}

pub fn list(dir: &Path) -> Result<Vec<Entry>> {
    with_index(dir, |db| {
        let mut query = db.prepare(
            "SELECT id,title,started,updated,messages FROM sessions WHERE messages>0
             ORDER BY updated_ns DESC,id DESC LIMIT ?1",
        )?;
        let rows = query.query_map([MAX_LIST], row_entry)?;
        let entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries.into_iter().filter(|entry| dir.join(format!("{}.json", entry.id)).is_file()).collect())
    })
}

pub fn latest(dir: &Path) -> Result<Option<Entry>> {
    with_index(dir, |db| {
        let mut query = db.prepare(
            "SELECT id,title,started,updated,messages FROM sessions WHERE messages>0
             ORDER BY updated_ns DESC,id DESC LIMIT ?1",
        )?;
        let rows = query.query_map([MAX_LIST], row_entry)?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .find(|entry| dir.join(format!("{}.json", entry.id)).is_file()))
    })
}

/// The session with this id, else the sessions whose id starts with it:
/// one is a match, more is ambiguous, none is not found.
pub fn find(dir: &Path, key: &str) -> Result<Vec<Entry>> {
    if key.is_empty() {
        return Ok(Vec::new());
    }
    with_index(dir, |db| {
        let exact: Option<Entry> = db
            .query_row("SELECT id,title,started,updated,messages FROM sessions WHERE id=?1", [key], row_entry)
            .optional()?;
        if let Some(entry) = exact {
            return Ok(vec![entry]);
        }
        let upper = format!("{key}\u{10ffff}");
        let mut stmt = db.prepare(
            "SELECT id,title,started,updated,messages FROM sessions WHERE id>=?1 AND id<?2 ORDER BY id LIMIT ?3",
        )?;
        let hits = stmt.query_map(params![key, upper, MAX_MATCHES], row_entry)?;
        let hits = hits.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(hits.into_iter().filter(|entry| dir.join(format!("{}.json", entry.id)).is_file()).collect())
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
             FROM session_terms f JOIN sessions s ON s.n=f.rowid
             WHERE session_terms MATCH ?1 ORDER BY bm25(session_terms) LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![expression, MAX_MATCHES], row_entry)?;
        let entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries.into_iter().filter(|entry| dir.join(format!("{}.json", entry.id)).is_file()).collect())
    })
}

pub fn render(entries: &[Entry], now: u64) -> String {
    render_at(entries, now, crate::ui::width())
}

/// `text` cut to `room` columns, ending in "…" when something was cut.
fn fit(text: &str, room: usize) -> String {
    if UnicodeWidthStr::width(text) <= room {
        return text.to_string();
    }
    let ellipsis_width = UnicodeWidthChar::width('…').unwrap_or(1);
    let prefix_room = room.saturating_sub(ellipsis_width);
    let mut used = 0;
    let mut kept = String::new();
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > prefix_room {
            break;
        }
        kept.push(ch);
        used += ch_width;
    }
    if room < ellipsis_width {
        return kept.trim_end().to_owned();
    }
    format!("{}…", kept.trim_end())
}

/// Each session in two lines that fit `width` columns: its id (a prefix
/// still finds it when the id is cut), then age, size and title.
fn render_at(entries: &[Entry], now: u64, width: usize) -> String {
    if entries.is_empty() {
        return format!("  {}\n", crate::ui::dim("no sessions here yet"));
    }
    let mut output = String::new();
    for entry in entries {
        let title = if entry.title.is_empty() { "(untitled)" } else { &entry.title };
        let detail = format!("    {:>4}  {:>4} msgs  ", age(entry.updated, now), entry.messages);
        let room = width.saturating_sub(UnicodeWidthStr::width(detail.as_str()));
        output.push_str(&format!(
            "  {}\n{detail}{}\n",
            crate::ui::info(&fit(&entry.id, width.saturating_sub(2))),
            fit(title, room)
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
        assert!(!a.with_extension("lock").exists(), "a released lease leaves no lock file behind");
        assert!(Lease::acquire(&a).is_ok());
        assert!(!a.with_extension("pending").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_lock_file_unlinked_by_its_releasing_holder_is_never_shared() {
        let dir = temp_dir("lease-unlink");
        let transcript = dir.join("raced.json");
        let lease = Lease::acquire(&transcript).unwrap();
        // A second process opened the lock file just before the holder left.
        let stale = File::open(transcript.with_extension("lock")).unwrap();
        drop(lease);
        assert_eq!(unsafe { libc::flock(stale.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        // Locking the orphaned inode grants nothing: the writer is whoever
        // holds the file at the path, and there is exactly one of those.
        let writer = Lease::acquire(&transcript).unwrap();
        assert!(Lease::acquire(&transcript).is_err());
        drop(writer);
        drop(stale);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn repeated_saves_skip_clean_messages_but_always_scrub_secrets() {
        let dir = temp_dir("scrub-memo");
        let path = dir.join("memo.json");
        let clean = serde_json::json!({"role":"tool","content":"fn main() { println!(\"memo-clean\"); }"});
        let secret = serde_json::json!({"role":"tool","content":"password = MEMO_SECRET_CANARY_41"});
        let saved = |history: Vec<serde_json::Value>| {
            save(&path, serde_json::json!({"history":history,"plan":["token = MEMO_PLAN_CANARY_77"]})).unwrap();
            std::fs::read_to_string(&path).unwrap()
        };
        for _ in 0..2 {
            let text = saved(vec![clean.clone(), secret.clone()]);
            assert!(text.contains("memo-clean"), "{text}");
            assert!(!text.contains("MEMO_SECRET_CANARY_41") && !text.contains("MEMO_PLAN_CANARY_77"), "{text}");
        }
        // A remembered clean message that changes is scanned again.
        let mut leaked = clean.clone();
        leaked["content"] = serde_json::json!("fn main() {}\napi_key = MEMO_CHANGED_CANARY_93");
        let text = saved(vec![leaked, secret.clone()]);
        assert!(!text.contains("MEMO_CHANGED_CANARY_93"), "{text}");
        let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(saved["history"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn large_transcripts_are_compressed_private_and_load_like_plain_ones() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("compressed");
        let path = dir.join("large.json");
        let output = "fn compressed_tool_output() {}\n".repeat(COMPRESS_FROM / 16);
        let history = vec![
            serde_json::json!({"role":"user","content":"read the large file"}),
            serde_json::json!({"role":"tool","content":format!("{output}secret = GZIP_SECRET_CANARY_58")}),
        ];
        save(&path, serde_json::json!({"history":history})).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(&[0x1f, 0x8b]), "a large transcript is saved compressed");
        assert!(bytes.len() < output.len() / 4);
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let text = read(&path).unwrap();
        assert!(!text.contains("GZIP_SECRET_CANARY_58") && text.contains("compressed_tool_output"));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["history"].as_array().unwrap().len(), 2);
        // Catalog discovery and recovery read compressed transcripts too.
        assert_eq!(search(&dir, "large").unwrap().len(), 1);

        // Plain transcripts, small or written by an older rusty, still load.
        let plain = dir.join("plain.json");
        save(&plain, serde_json::json!({"history":[{"role":"user","content":"small plain ask"}]})).unwrap();
        assert!(std::fs::read_to_string(&plain).unwrap().starts_with('{'));
        assert!(read(&plain).unwrap().contains("small plain ask"));
        assert_eq!(search(&dir, "plain").unwrap().len(), 1);
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
        drop(index_ready(&dir, true).unwrap());
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
    fn a_session_file_that_appears_after_the_index_is_picked_up() {
        let dir = temp_dir("late-file");
        assert!(list(&dir).unwrap().is_empty());
        let late = dir.join("1700000001-42.json");
        let history = serde_json::json!({"history":[{"role":"user","content":"fix the websocket reconnect"}]});
        rusty::privacy::write_json(&late, history).unwrap();
        rusty::privacy::write_json(&dir.join("reserved.json"), serde_json::json!({})).unwrap();
        let all = list(&dir).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!((all[0].id.as_str(), all[0].title.as_str()), ("1700000001-42", "fix the websocket reconnect"));
        assert_eq!(search(&dir, "websocket").unwrap().len(), 1);
        assert_eq!(find(&dir, "1700000001").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_session_saves_do_not_backfill_the_catalog() {
        let dir = temp_dir("incremental-save");
        assert!(list(&dir).unwrap().is_empty()); // Initialize an empty catalog.
        let late = dir.join("1700000001-42.json");
        rusty::privacy::write_json(
            &late,
            serde_json::json!({"history":[{"role":"user","content":"imported websocket reconnect"}]}),
        )
        .unwrap();

        let current = create(&dir).unwrap();
        let lease = Lease::acquire(&current).unwrap();
        let history = vec![serde_json::json!({"role":"user","content":"current session ask"})];
        publish_and_record(&dir, &lease, serde_json::json!({"history":history}), &history, &[]).unwrap();
        let index = Connection::open(dir.join(INDEX)).unwrap();
        let imported: i64 =
            index.query_row("SELECT count(*) FROM sessions WHERE id='1700000001-42'", [], |row| row.get(0)).unwrap();
        assert_eq!(imported, 0, "a routine save updates its own row without scanning unrelated files");

        let found = search(&dir, "websocket").unwrap();
        assert_eq!(found.len(), 1, "catalog reads still discover copied-in legacy files");
        assert_eq!(found[0].id, "1700000001-42");
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
        // before its SQLite transaction; the marker goes once it is indexed.
        let old = dir.join("old.json");
        rusty::privacy::write_json(
            &old,
            serde_json::json!({"history":[{"role":"user","content":"old marker phrase"}]}),
        )
        .unwrap();
        drop(index_ready(&dir, true).unwrap());

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
    fn one_marker_covers_a_writers_saves_and_goes_when_it_leaves_indexed() {
        let dir = temp_dir("writer-marker");
        let transcript = create(&dir).unwrap();
        let marker = dir.join(PENDING_DIR).join(id_of(&transcript));
        let lease = Lease::acquire(&transcript).unwrap();
        for word in ["first", "second"] {
            let history = vec![serde_json::json!({"role":"user","content":format!("{word} marker save")})];
            publish_and_record(&dir, &lease, serde_json::json!({"history":history}), &history, &[]).unwrap();
            assert!(marker.exists(), "the marker stays while its writer holds the lease");
        }
        assert_eq!(search(&dir, "second").unwrap().len(), 1);
        drop(lease);
        assert!(!marker.exists(), "a writer whose last save is indexed clears its marker");
        assert!(!transcript.with_extension("lock").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_writer_that_dies_between_rename_and_commit_is_recovered() {
        let dir = temp_dir("writer-crash");
        let transcript = create(&dir).unwrap();
        let lease = Lease::acquire(&transcript).unwrap();
        let old = vec![serde_json::json!({"role":"user","content":"old granite phrase"})];
        publish_and_record(&dir, &lease, serde_json::json!({"history":old}), &old, &[]).unwrap();
        // The next save renames its transcript and dies before its commit.
        lease.indexed.store(false, Ordering::Release);
        let new = vec![serde_json::json!({"role":"user","content":"new basalt phrase"})];
        save(&transcript, serde_json::json!({"history":new})).unwrap();
        drop(lease);
        assert!(dir.join(PENDING_DIR).join(id_of(&transcript)).exists());

        assert_eq!(search(&dir, "basalt").unwrap().len(), 1);
        assert!(search(&dir, "granite").unwrap().is_empty());
        assert!(!dir.join(PENDING_DIR).join(id_of(&transcript)).exists());
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
        drop(index_ready(&dir, true).unwrap());
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

        let mut index = index_ready(&dir, true).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        let entry = Entry {
            id: id_of(&newer),
            title: "newer existing row".into(),
            started: 1,
            updated: newer_ns / 1_000_000_000,
            messages: 1,
        };
        upsert(&tx, &entry, newer_ns, Some("newer existing row")).unwrap();
        tx.commit().unwrap();
        drop(index);
        pending_marker(&dir, &older).unwrap();

        let newest = latest(&dir).unwrap().unwrap();
        assert_eq!(newest.id, id_of(&newer));
        let index = index_ready(&dir, true).unwrap();
        let older_indexed_ns: u64 = index
            .db
            .query_row("SELECT updated_ns FROM sessions WHERE id='older-pending'", [], |row| row.get(0))
            .unwrap();
        assert_eq!(older_indexed_ns, older_ns, "recovery preserves the transcript's actual mtime");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_version_2_index_upgrades_in_place_and_keeps_its_rows() {
        let dir = temp_dir("index-v2");
        // A transcript whose history no longer opens with the title the
        // catalog kept: a rebuild from files would lose that title.
        let kept = dir.join("kept.json");
        rusty::privacy::write_json(&kept, serde_json::json!({"history":[{"role":"user","content":"later ask"}]}))
            .unwrap();
        let db = Connection::open(dir.join(INDEX)).unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(
               id TEXT PRIMARY KEY, title TEXT NOT NULL, started INTEGER NOT NULL,
               updated INTEGER NOT NULL, updated_ns INTEGER NOT NULL, messages INTEGER NOT NULL,
               text TEXT NOT NULL, terms TEXT NOT NULL
             );
             CREATE INDEX sessions_updated ON sessions(updated_ns DESC, id DESC);
             CREATE VIRTUAL TABLE session_terms USING fts5(id UNINDEXED, terms, tokenize='unicode61');
             INSERT INTO sessions VALUES('kept','fix the zebra bug',5,6,6000000000,3,'fix the zebra bug','fix zebra bug');
             INSERT INTO session_terms(rowid,id,terms) SELECT rowid,id,terms FROM sessions;
             PRAGMA user_version=2;",
        )
        .unwrap();
        drop(db);

        let listed = list(&dir).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (listed[0].title.as_str(), listed[0].started, listed[0].updated, listed[0].messages),
            ("fix the zebra bug", 5, 6, 3)
        );
        assert_eq!(search(&dir, "zebra").unwrap().len(), 1, "terms indexed by version 2 stay searchable");
        let db = Connection::open(dir.join(INDEX)).unwrap();
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0)).unwrap();
        assert_eq!(version, INDEX_VERSION);
        assert!(db.prepare("SELECT text FROM sessions").is_err(), "the duplicated text column is gone");
        drop(db);

        // Later saves of the upgraded row replace its terms.
        let history = vec![serde_json::json!({"role":"user","content":"now about the yak"})];
        record(&dir, &kept, &history, &[]).unwrap();
        assert!(search(&dir, "zebra").unwrap().len() == 1, "the kept title is still indexed");
        assert_eq!(search(&dir, "yak").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_save_with_nothing_said_keeps_the_indexed_terms() {
        let dir = temp_dir("terms-kept");
        let path = create(&dir).unwrap();
        record(&dir, &path, &[serde_json::json!({"role":"user","content":"tune the walrus cache"})], &[]).unwrap();
        // A compacted history can hold only notes and replies.
        record(&dir, &path, &[serde_json::json!({"role":"assistant","content":"done"})], &[]).unwrap();
        assert_eq!(search(&dir, "walrus").unwrap().len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_first_title_stays_when_the_history_no_longer_starts_with_it() {
        let dir = temp_dir("title-kept");
        let path = create(&dir).unwrap();
        let ask = |text: &str| serde_json::json!({"role":"user","content":text});
        let first = vec![ask("fix the zebra bug"), serde_json::json!({"role":"assistant","content":"ok"})];
        record(&dir, &path, &first, &[]).unwrap();
        // Resumed after a compaction: the history now opens with a later ask.
        record(&dir, &path, &[ask("and the yak?")], &[]).unwrap();
        let entry = find(&dir, &id_of(&path)).unwrap().remove(0);
        assert_eq!((entry.title.as_str(), entry.messages), ("fix the zebra bug", 1));
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
        let rendered = render(std::slice::from_ref(&entry), 120);
        assert!(rendered.starts_with("  0123456789abcdef0123456789abcdef\n"));
        assert!(rendered.contains("    now    12 msgs  A useful session\n"));
        let long = Entry { title: "fix the second flaky test in the parser module today".into(), ..entry };
        for width in [30, 40, 80] {
            let rendered = render_at(std::slice::from_ref(&long), 120, width);
            for line in rendered.lines() {
                assert!(UnicodeWidthStr::width(line) <= width, "{width}: {line:?}");
            }
            assert_eq!(rendered.contains('…'), width < 72, "{width}: {rendered}");
        }
        assert!(render_at(std::slice::from_ref(&long), 120, 30).starts_with("  0123456789abcdef0123456789a…\n"));

        for title in ["界界界界界界界界界界", "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}"] {
            let entry = Entry { title: title.into(), ..long.clone() };
            let rendered = render_at(std::slice::from_ref(&entry), 120, 30);
            for line in rendered.lines() {
                assert!(UnicodeWidthStr::width(line) <= 30, "30: {line:?}");
            }
            assert!(rendered.contains('…'), "{rendered}");
        }
        assert_eq!(fit("界界界", 4), "界…");
        assert_eq!(fit("e\u{301}xyz", 3), "e\u{301}x…");
    }

    #[test]
    fn large_catalog_search_uses_fts_and_caps_results() {
        let dir = temp_dir("large");
        let mut index = index_ready(&dir, true).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        for n in 0..5_000 {
            let id = format!("s{n:05}");
            rusty::privacy::write_json(&dir.join(format!("{id}.json")), serde_json::json!({})).unwrap();
            let text = if n == 4_999 { "needle_unique_token searchable catalog" } else { "ordinary catalog entry" };
            let entry = Entry { id, title: text.to_owned(), started: 1, updated: n, messages: 2 };
            upsert(&tx, &entry, n * 1_000_000_000, Some(text)).unwrap();
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

        let index = index_ready(&dir, true).unwrap();
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
        let mut index = index_ready(&dir, true).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        for (id, updated_ns) in [("older", 1_800_000_000_100_000_000), ("newer", 1_800_000_000_900_000_000)] {
            rusty::privacy::write_json(&dir.join(format!("{id}.json")), serde_json::json!({})).unwrap();
            let entry = Entry { id: id.into(), title: id.into(), started: 10, updated: 1_800_000_000, messages: 2 };
            upsert(&tx, &entry, updated_ns, Some(id)).unwrap();
        }
        tx.commit().unwrap();
        drop(index);

        let newest = latest(&dir).unwrap().unwrap();
        assert_eq!(newest.id, "newer");
        assert_eq!(newest.updated, 1_800_000_000);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_shared_id_prefix_finds_every_session_that_has_it() {
        let dir = temp_dir("prefix");
        let mut index = index_ready(&dir, true).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        for (n, id) in ["aa11", "aa22", "bb33"].into_iter().enumerate() {
            rusty::privacy::write_json(&dir.join(format!("{id}.json")), serde_json::json!({})).unwrap();
            let entry = Entry { id: id.into(), title: id.into(), started: 1, updated: 1, messages: 2 };
            upsert(&tx, &entry, n as u64, Some(id)).unwrap();
        }
        tx.commit().unwrap();
        drop(index);
        let ids = |key: &str| find(&dir, key).unwrap().into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(ids("aa"), ["aa11", "aa22"]);
        assert_eq!(ids("aa2"), ["aa22"]);
        assert_eq!(ids("bb33"), ["bb33"]);
        assert!(ids("cc").is_empty() && ids("").is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_empty_session_is_never_listed_or_latest() {
        let dir = temp_dir("empty-latest");
        let mut index = index_ready(&dir, true).unwrap();
        let tx = index.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        // An older build indexed a REPL opened and closed without a turn.
        for (id, messages, ns) in [("spoken", 2, 1_000), ("silent", 0, 2_000)] {
            rusty::privacy::write_json(&dir.join(format!("{id}.json")), serde_json::json!({})).unwrap();
            let entry = Entry { id: id.into(), title: String::new(), started: 1, updated: 1, messages };
            upsert(&tx, &entry, ns, Some("")).unwrap();
        }
        tx.commit().unwrap();
        drop(index);
        assert_eq!(latest(&dir).unwrap().unwrap().id, "spoken");
        assert_eq!(list(&dir).unwrap().iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["spoken"]);
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
        let title: String = index.query_row("SELECT title FROM sessions WHERE id=?1", [&id], |row| row.get(0)).unwrap();
        // The indexed terms come from this redacted text.
        let text = text_for(&[], &history);
        assert!(text.contains("let token = self.token.clone();"), "{text}");
        assert!(text.contains("password=[redacted]"), "{text}");
        assert!(!title.contains(secret) && !text.contains(secret));
        assert!(search(&dir, secret).unwrap().is_empty());
        assert!(search(&dir, "self.token.clone").unwrap().iter().any(|entry| entry.id == id));
        assert!(!render(&list(&dir).unwrap(), rusty::unix_now()).contains(secret));
        let bytes = std::fs::read(dir.join(INDEX)).unwrap();
        assert!(!bytes.windows(secret.len()).any(|window| window == secret.as_bytes()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
