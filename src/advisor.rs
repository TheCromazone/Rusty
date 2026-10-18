//! The memory advisor behind `rusty-memoryd`. The agent's hooks have a
//! deadline; the daemon never runs tools or calls a model.
//!
//! A lesson is offered when relevance × confidence × freshness clears the
//! level's threshold. Confidence moves only on a goal's verified check or a
//! person's `/memory helpful|harmful`, never on being shown. Preferences are
//! not ranked: they apply everywhere, so `begin` returns them for the prompt.
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

const FRAME: usize = 32_768;
const TRANSFER: usize = 4 * 1024 * 1024;
const LESSONS: usize = 512;
const SESSIONS: usize = 32;
const HOOK_MS: u64 = 50;
/// Recent tool observations kept per session for recall.
const OBSERVATIONS: usize = 4;
/// Lessons a client remembers offering, for crediting a goal.
const DELIVERED: usize = 16;
pub const KINDS: [&str; 5] = ["preference", "fact", "decision", "gotcha", "todo"];
const SOURCES: [&str; 4] = ["user", "agent", "compaction", "reflect"];
/// The scope for preferences that apply to every project.
const GLOBAL: &str = "global";
/// BM25 becomes a 0-1 relevance as s / (s + 2): one shared everyday term
/// gives 0.33, three give 0.6, one rare term more.
const RELEVANCE_HALF: f64 = 2.0;
/// Two lessons of one kind sharing this much of the shorter one's terms are one lesson.
const DUPLICATE: f64 = 0.7;

/// How much memory does. Each level includes the ones before it; higher
/// levels capture more, offer lessons more readily and prune harder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// No memory at all.
    Off,
    /// Offers saved lessons that match the request.
    Recall,
    /// Also reads recent tool output, checks lessons against their files and
    /// learns from a goal's fixed check.
    #[default]
    Learn,
    /// Also asks the model, after a checked goal, which lessons it used, and
    /// keeps short new lessons from passes and from compaction.
    Reflect,
    /// The most aggressive: more lessons offered, more kept, faster pruning.
    Deep,
}

impl Level {
    pub const NAMES: &'static str = "off, recall, learn, reflect, deep";

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "off" => Self::Off,
            "recall" => Self::Recall,
            "learn" => Self::Learn,
            "reflect" => Self::Reflect,
            "deep" => Self::Deep,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Recall => "recall",
            Self::Learn => "learn",
            Self::Reflect => "reflect",
            Self::Deep => "deep",
        }
    }
    /// Runs the advisor at all.
    pub fn advises(self) -> bool {
        self >= Self::Recall
    }
    /// Recent tool output joins the request when choosing a lesson.
    pub fn observes(self) -> bool {
        self >= Self::Learn
    }
    /// Lessons record the files they describe and are re-checked before use.
    pub fn anchors(self) -> bool {
        self >= Self::Learn
    }
    /// A goal's fixed check credits or charges the lessons it was offered.
    pub fn credits(self) -> bool {
        self >= Self::Learn
    }
    /// One budgeted model request after a checked goal; compaction lessons kept.
    pub fn reflects(self) -> bool {
        self >= Self::Reflect
    }
    /// New lessons reflection may keep from one passing goal.
    pub fn new_lessons(self) -> usize {
        match self {
            Self::Deep => 3,
            Self::Reflect => 2,
            _ => 0,
        }
    }
    /// The lowest relevance × confidence × freshness worth offering.
    fn threshold(self) -> f64 {
        match self {
            Self::Recall => 0.25,
            Self::Deep => 0.10,
            _ => 0.15,
        }
    }
    fn per_turn(self) -> usize {
        match self {
            Self::Recall => 1,
            Self::Deep => 3,
            _ => 2,
        }
    }
    /// A lesson with two or more outcomes goes when its posterior mean falls below this.
    fn retire_below(self) -> f64 {
        match self {
            Self::Reflect => 0.35,
            Self::Deep => 0.4,
            _ => 0.3,
        }
    }
    /// Days a lesson nobody vouched for may go without a success.
    fn expiry_days(self) -> u64 {
        match self {
            Self::Reflect => 21,
            Self::Deep => 14,
            _ => 28,
        }
    }
}

pub fn clip(s: &str, bytes: usize) -> String {
    let mut n = bytes.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    s[..n].to_owned()
}

fn listing_header(op: &str, shown: usize, total: usize) -> String {
    let matching = op == "recall";
    let mut header =
        format!("Showing {shown} of {total} {}.", if matching { "matching saved entries" } else { "saved entries" });
    if shown < total {
        header.push_str(if matching {
            " More matches were omitted; use a narrower search query to see additional entries."
        } else {
            " More entries were omitted to keep this list bounded; use a search query to narrow it."
        });
    }
    header
}

fn bounded_listing(op: &str, lines: &[String], total: usize, bytes: usize) -> String {
    let mut shown = Vec::new();
    let mut lines_len = 0;
    for line in lines {
        let next = shown.len() + 1;
        // Header newline plus one separator newline per entry. Keep complete
        // records so the final line can never look like a complete memory if
        // its text was cut off by the response cap.
        if listing_header(op, next, total).len() + lines_len + line.len() + next > bytes {
            break;
        }
        lines_len += line.len();
        shown.push(line);
    }

    let header = listing_header(op, shown.len(), total);
    if shown.is_empty() {
        header
    } else {
        format!("{header}\n{}", shown.into_iter().map(String::as_str).collect::<Vec<_>>().join("\n"))
    }
}
use crate::{fnv as hash, unix_now as now};
fn lesson_id(scope: &str, kind: &str, text: &str) -> String {
    format!("{:016x}", hash(&format!("{scope}:{kind}:{text}")))
}

/// The worktree's top level (where anchored paths start), or `cwd` outside git.
fn worktree(cwd: &Path) -> PathBuf {
    Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|p| PathBuf::from(p.trim()))
        .unwrap_or_else(|| cwd.to_owned())
}

/// Same repository across worktrees; explicit identity also works across cloud clones.
pub fn scope(cwd: &Path) -> String {
    if let Ok(id) = std::env::var("RUSTY_PROJECT_ID") {
        if !id.trim().is_empty() {
            return format!("project:{:016x}", hash(id.trim()));
        }
    }
    let out = Command::new("git").args(["rev-parse", "--git-common-dir"]).current_dir(cwd).output();
    let root = out
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let p = String::from_utf8(o.stdout).ok()?;
            cwd.join(p.trim()).canonicalize().ok()
        })
        .unwrap_or_else(|| cwd.to_owned());
    format!("project:{:016x}", hash(&root.to_string_lossy()))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub op: String,
    pub scope: String,
    pub session: String,
    pub seq: u64,
    pub level: Level,
    #[serde(default)]
    pub data: Value,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Response {
    pub text: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub error: bool,
    #[serde(default)]
    pub data: Value,
}

/// How a successful `remember` reply starts; anything else is a refusal.
pub const SAVED_PREFIX: &str = "saved memory ";
/// What `remember` answers when a near-duplicate was already on file and
/// only its anchors were refreshed.
pub const KNOWN_PREFIX: &str = "already saved ";
pub fn rpc(socket: &Path, req: &Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    // Hooks still return after HOOK_MS. Explicit management may flush SQLite
    // or transfer a snapshot, so its transport must honor the control budget.
    let transport_ms = if matches!(req.op.as_str(), "begin" | "event" | "finish" | "advice") { 200 } else { 1000 };
    stream.set_read_timeout(Some(Duration::from_millis(transport_ms)))?;
    stream.set_write_timeout(Some(Duration::from_millis(transport_ms)))?;
    let mut body = serde_json::to_vec(req)?;
    if body.len() > if req.op == "import" { TRANSFER } else { FRAME } {
        bail!("memory frame too large");
    }
    body.push(b'\n');
    stream.write_all(&body)?;
    let mut text = String::new();
    BufReader::new(stream.take(TRANSFER as u64)).read_line(&mut text)?;
    Ok(serde_json::from_str(&text)?)
}

#[derive(Default, Serialize)]
pub struct Metrics {
    pub hooks: u64,
    pub timeouts: u64,
    pub injections: u64,
    pub hook_us: u64,
    pub max_hook_us: u64,
    /// Each lesson offered, with the index in the trajectory's `usage` of the
    /// first request that carried it.
    pub offered: Vec<Offered>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Offered {
    pub usage_index: usize,
    pub id: String,
    pub text: String,
}
type Work = (Request, mpsc::Sender<Result<Response>>);
/// The agent's side of the advisor.
pub struct Hooks {
    tx: mpsc::SyncSender<Work>,
    pub level: Level,
    scope: String,
    session: String,
    seq: u64,
    /// Where the agent runs, and the worktree root anchors are relative to.
    cwd: PathBuf,
    root: PathBuf,
    /// Lessons offered since `take_delivered`, as (id, lesson text).
    delivered: Vec<(String, String)>,
    /// The preferences block from the last `begin` that answered in time.
    preferences: String,
    /// Lessons offered since `begin`. They stay in the prompt for the rest of
    /// the turn: the service offers a lesson once, and a lesson seen on one
    /// request only is gone by the step that needs it.
    turn_advice: Vec<String>,
    pub metrics: Metrics,
}
impl Hooks {
    pub fn connect(home: &Path, cwd: &Path, level: Level) -> Result<Self> {
        let socket = home.join("memory/advisor.sock");
        if socket.as_os_str().len() > 100 {
            bail!("RUSTY_HOME is too long for a local memory socket");
        }
        if UnixStream::connect(&socket).is_err() {
            let bin = std::env::current_exe()?.with_file_name("rusty-memoryd");
            let mut child = Command::new(bin)
                .arg("--home")
                .arg(home)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("install rusty-memoryd alongside rusty")?;
            let start = Instant::now();
            while UnixStream::connect(&socket).is_err() {
                if child.try_wait()?.is_some() || start.elapsed() > Duration::from_secs(2) {
                    bail!("memory daemon did not start");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let (tx, rx) = mpsc::sync_channel::<Work>(8);
        std::thread::spawn(move || {
            for (req, reply) in rx {
                let _ = reply.send(rpc(&socket, &req));
            }
        });
        Ok(Self {
            tx,
            level,
            scope: scope(cwd),
            session: format!("{}-{}", std::process::id(), now()),
            seq: 0,
            cwd: cwd.to_owned(),
            root: worktree(cwd).canonicalize().unwrap_or_else(|_| cwd.to_owned()),
            delivered: Vec::new(),
            preferences: String::new(),
            turn_advice: Vec::new(),
            metrics: Metrics::default(),
        })
    }
    fn call(&mut self, op: &str, data: Value, ms: u64) -> Option<Response> {
        let start = Instant::now();
        let req = Request {
            op: op.into(),
            scope: self.scope.clone(),
            session: self.session.clone(),
            seq: self.seq,
            level: self.level,
            data,
        };
        let (tx, rx) = mpsc::channel();
        let result = self
            .tx
            .try_send((req, tx))
            .ok()
            .and_then(|_| rx.recv_timeout(Duration::from_millis(ms)).ok())
            .and_then(Result::ok);
        self.metrics.hooks += 1;
        let us = start.elapsed().as_micros() as u64;
        self.metrics.hook_us += us;
        self.metrics.max_hook_us = self.metrics.max_hook_us.max(us);
        if result.is_none() {
            self.metrics.timeouts += 1;
        }
        result
    }
    pub fn begin(&mut self, request: &str) {
        self.seq += 1;
        self.turn_advice.clear();
        let r = self.call("begin", json!({"text":clip(request,4096),"root":self.root.to_string_lossy()}), HOOK_MS);
        if let Some(r) = r.filter(|r| !r.error) {
            self.preferences = r.data["preferences"].as_str().unwrap_or("").to_string();
        }
    }
    /// The user's preferences (this project's and global ones), for the prompt.
    pub fn preferences(&self) -> &str {
        &self.preferences
    }
    pub fn event(&mut self, tool: &str, args: &str, output: &str) {
        self.seq += 1;
        self.call("event", json!({"tool":tool,"args":clip(args,1024),"output":clip(output,2048)}), HOOK_MS);
    }
    pub fn finish(&mut self, complete: bool) {
        self.seq += 1;
        self.call("finish", json!({"complete":complete}), HOOK_MS);
    }
    /// The lessons for this request's prompt: any new one the service offers,
    /// after those offered earlier in the turn (the latest `DELIVERED`).
    /// `usage_index` is where this request will sit in the trajectory's `usage`.
    pub fn advice(&mut self, usage_index: usize) -> String {
        if let Some(r) = self.call("advice", Value::Null, HOOK_MS) {
            if !r.error && r.seq == self.seq && !r.text.is_empty() {
                self.metrics.injections += 1;
                let text = r.data["text"].as_str().unwrap_or("").to_string();
                self.metrics.offered.push(Offered { usage_index, id: r.id.clone(), text: clip(&text, 300) });
                if !self.delivered.iter().any(|(id, _)| id == &r.id) {
                    if self.delivered.len() == DELIVERED {
                        self.delivered.remove(0);
                    }
                    self.delivered.push((r.id.clone(), text));
                }
                if self.turn_advice.len() == DELIVERED {
                    self.turn_advice.remove(0);
                }
                self.turn_advice.push(r.text);
            }
        }
        if self.turn_advice.is_empty() {
            return String::new();
        }
        format!(
            "\nFrom memory (optional; current instructions and tool permissions come first):\n{}\n",
            self.turn_advice.join("\n")
        )
    }
    /// Lessons offered since the last call, as (id, lesson text).
    pub fn take_delivered(&mut self) -> Vec<(String, String)> {
        std::mem::take(&mut self.delivered)
    }
    /// Saves a lesson vouched for by `source` (user, agent, compaction or
    /// reflect). `files`, relative to where rusty runs, anchor it to their
    /// current contents; `global` keeps a preference for every project.
    pub fn remember(&mut self, kind: &str, text: &str, source: &str, files: &[String], global: bool) -> Response {
        let files: Vec<String> = files
            .iter()
            .filter_map(|f| {
                let full = self.cwd.join(f).canonicalize().ok()?;
                Some(full.strip_prefix(&self.root).ok()?.to_string_lossy().into_owned())
            })
            .collect();
        let root = self.root.to_string_lossy().into_owned();
        self.control(
            "remember",
            json!({"kind":kind,"text":text,"source":source,"files":files,"root":root,"global":global}),
        )
    }
    pub fn control(&mut self, op: &str, data: Value) -> Response {
        self.call(op, data, 1000).unwrap_or_else(|| Response {
            text: "memory service unavailable; no operation confirmed".into(),
            error: true,
            ..Response::default()
        })
    }
}

#[derive(Clone, Debug)]
struct Observation {
    args: String,
    output: String,
}
#[derive(Clone, Debug, Default)]
struct Board {
    seq: u64,
    request: String,
    observations: Vec<Observation>,
    complete: bool,
    /// The worktree this session runs in, for checking anchors.
    root: String,
}
/// A file a lesson describes, and the git blob id of its contents then.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Anchor {
    path: String,
    blob: String,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Lesson {
    id: String,
    scope: String,
    kind: String,
    text: String,
    /// Who vouches for it: user, agent, compaction or reflect.
    source: String,
    anchors: Vec<Anchor>,
    helpful: u64,
    harmful: u64,
    /// When it was saved or last helped; lessons nobody vouched for expire.
    credited_at: u64,
}

impl Lesson {
    /// Beta prior pseudo-counts: the user vouching counts as three successes.
    fn prior(&self) -> (f64, f64) {
        if self.source == "user" {
            (3.0, 1.0)
        } else {
            (1.0, 1.0)
        }
    }
    /// The Beta posterior's mean less half its standard deviation: a cautious
    /// estimate that ranks nine successes in ten above one in one, and moves
    /// only with outcomes.
    fn confidence(&self) -> f64 {
        let (a0, b0) = self.prior();
        let (a, b) = (a0 + self.helpful as f64, b0 + self.harmful as f64);
        let sd = (a * b / ((a + b) * (a + b) * (a + b + 1.0))).sqrt();
        (a / (a + b) - 0.5 * sd).max(0.0)
    }
}

/// Lessons a level retires: outcomes say they mostly hurt (the posterior
/// mean, with the same prior as `Lesson::prior`), or nobody vouched for them
/// and they earned nothing in the level's expiry. ?2 is the mean, ?3 the cutoff.
const RETIRED: &str = "(helpful + harmful >= 2
      AND (CASE source WHEN 'user' THEN 3.0 ELSE 1.0 END + helpful)
          / (CASE source WHEN 'user' THEN 4.0 ELSE 2.0 END + helpful + harmful) < ?2)
   OR (source != 'user' AND helpful = 0 AND credited_at < ?3)";

/// A relative path that stays inside the worktree: no `..`, no root.
fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 256
        && Path::new(path).components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Git blob ids of anchored files, cached by path, size and mtime so a hook
/// only `stat`s a file whose contents it has already hashed.
#[derive(Default)]
struct Blobs(HashMap<PathBuf, (u64, SystemTime, String)>);
impl Blobs {
    /// Git's blob id (SHA-1 of `blob <len>\0` and the bytes) of a regular
    /// file under `root`, refusing symlinks and files over 1 MiB.
    fn get(&mut self, root: &Path, path: &str) -> Option<String> {
        if !safe_path(path) {
            return None;
        }
        let full = root.join(path);
        if !full.canonicalize().ok()?.starts_with(root.canonicalize().ok()?) {
            return None;
        }
        let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&full).ok()?;
        let meta = f.metadata().ok()?;
        let mtime = meta.modified().ok()?;
        if !meta.is_file() || meta.len() > 1 << 20 {
            return None;
        }
        if let Some((len, at, blob)) = self.0.get(&full) {
            if *len == meta.len() && *at == mtime {
                return Some(blob.clone());
            }
        }
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        f.read_to_end(&mut bytes).ok()?;
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY);
        ctx.update(format!("blob {}\0", bytes.len()).as_bytes());
        ctx.update(&bytes);
        let blob: String = ctx.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect();
        if self.0.len() >= 4 * LESSONS {
            self.0.clear();
        }
        self.0.insert(full, (meta.len(), mtime, blob.clone()));
        Some(blob)
    }

    /// 1 when every anchor matches (or there are none), 0.5 when a file
    /// changed, 0 when one is gone; with the changed paths.
    fn freshness(&mut self, root: &str, anchors: &[Anchor]) -> (f64, Vec<String>) {
        if anchors.is_empty() || root.is_empty() {
            return (1.0, Vec::new());
        }
        let mut changed = Vec::new();
        for a in anchors {
            match self.get(Path::new(root), &a.path) {
                None => return (0.0, vec![a.path.clone()]),
                Some(b) if b != a.blob => changed.push(a.path.clone()),
                Some(_) => {}
            }
        }
        (if changed.is_empty() { 1.0 } else { 0.5 }, changed)
    }
}

/// What a session is about: the request, plus (when the level observes) the
/// last few tool calls' arguments and error lines. Only error lines (or a
/// first line) are kept, since whole outputs would drown the request.
fn query(board: &Board, observe: bool) -> HashSet<String> {
    let mut text = board.request.clone();
    if observe {
        for o in &board.observations {
            let errors: Vec<&str> = o
                .output
                .lines()
                .filter(|l| {
                    let l = l.to_lowercase();
                    l.contains("error") || l.contains("fail") || l.contains("not found")
                })
                .take(3)
                .collect();
            let lines = if errors.is_empty() { o.output.lines().take(1).collect() } else { errors };
            text.push_str(&format!(" {} {}", o.args, lines.join(" ")));
        }
    }
    crate::recall::query(&text)
}

/// Jaccard similarity of two lessons' term sets.
/// How much of the shorter lesson's wording the other one shares. Measured
/// against the smaller set rather than the union: two short facts that
/// differ by one word ("the repository's test command is …" against "the
/// project's test command is …") are the same lesson, and a Jaccard score
/// would put them under the duplicate line.
#[cfg(test)]
fn overlap(a: &str, b: &str) -> f64 {
    if polarity(a) != polarity(b) {
        return 0.0;
    }
    similarity(&duplicate_terms(a), &duplicate_terms(b))
}

/// Which of a few negations `text` uses, as bits: lessons that disagree on
/// them are never duplicates, and this is far cheaper than their terms.
fn polarity(text: &str) -> u8 {
    const NEGATIONS: [&str; 5] = ["no", "not", "never", "without", "cannot"];
    let normalized = text.to_ascii_lowercase().replace("n't", " not");
    normalized
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter_map(|word| NEGATIONS.iter().position(|n| *n == word))
        .fold(0, |mask, i| mask | 1 << i)
}

fn duplicate_terms(text: &str) -> HashSet<String> {
    crate::recall::query(text)
        .into_iter()
        .map(
            |term| {
                if matches!(term.as_str(), "repo" | "repository" | "project") {
                    "repo_project".to_string()
                } else {
                    term
                }
            },
        )
        .collect()
}

/// [`overlap`] of two lessons that agree on negation, from their terms.
fn similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let shared = a.intersection(b).count();
    // A strict subset can add a meaningful command option or value. Do not
    // collapse `run cargo test` with `run cargo test --release` just because
    // all the shorter fact's terms happen to occur in the longer one.
    if a != b && shared == a.len().min(b.len()) {
        return 0.0;
    }
    let union = a.union(b).count().max(1);
    shared as f64 / union as f64
}

struct Store {
    db: Connection,
}
const COLUMNS: &str = "id,scope,kind,text,source,anchors,helpful,harmful,credited_at";
const SCHEMA: i64 = 3;
impl Store {
    fn open(home: &Path) -> Result<Self> {
        let dir = home.join("memory");
        crate::privacy::private_dir(&dir)?;
        for name in ["memory.sqlite", "memory.sqlite-wal", "memory.sqlite-shm"] {
            let path = dir.join(name);
            if path.exists() || path.is_symlink() {
                let f = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
        }
        drop(
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(dir.join("memory.sqlite"))?,
        );
        let db = Connection::open(dir.join("memory.sqlite"))?;
        db.execute_batch(
            "PRAGMA secure_delete=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_size=-512; PRAGMA mmap_size=0;
             PRAGMA journal_mode=WAL; PRAGMA busy_timeout=1000; PRAGMA journal_size_limit=65536;
             PRAGMA wal_autocheckpoint=16; PRAGMA max_page_count=4096;",
        )?;
        // In WAL mode, NORMAL syncs at checkpoints rather than on every
        // commit: a power loss can drop the last committed lessons but never
        // corrupts the store. Outside WAL it could, so it stays FULL there.
        let journal: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if journal.eq_ignore_ascii_case("wal") {
            db.execute_batch("PRAGMA synchronous=NORMAL;")?;
        }
        // Stores from before this schema are dropped, not migrated.
        let version: i64 = db.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version != SCHEMA {
            db.execute_batch(&format!(
                "BEGIN;
                 DROP TABLE IF EXISTS lessons; DROP TABLE IF EXISTS deleted; DROP TABLE IF EXISTS policies;
                 DROP TABLE IF EXISTS events; DROP TABLE IF EXISTS boards;
                 CREATE TABLE lessons(id TEXT PRIMARY KEY, scope TEXT NOT NULL, kind TEXT NOT NULL,
                   text TEXT NOT NULL, source TEXT NOT NULL, anchors TEXT NOT NULL,
                   helpful INTEGER NOT NULL DEFAULT 0, harmful INTEGER NOT NULL DEFAULT 0,
                   credited_at INTEGER NOT NULL);
                 CREATE INDEX lessons_scope ON lessons(scope);
                 CREATE TABLE deleted(id TEXT PRIMARY KEY, scope TEXT NOT NULL);
                 PRAGMA user_version={SCHEMA};
                 COMMIT;"
            ))?;
        }
        Ok(Self { db })
    }
    fn lessons(&self, scope: &str) -> Result<Vec<Lesson>> {
        let mut q = self.db.prepare_cached(&format!("SELECT {COLUMNS} FROM lessons WHERE scope=?1"))?;
        let rows = q.query_map([scope], |r| {
            Ok(Lesson {
                id: r.get(0)?,
                scope: r.get(1)?,
                kind: r.get(2)?,
                text: r.get(3)?,
                source: r.get(4)?,
                anchors: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
                helpful: r.get(6)?,
                harmful: r.get(7)?,
                credited_at: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
    fn tombstoned(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM deleted WHERE id=?1)", [id], |r| r.get(0))?)
    }
    fn insert(&self, l: &Lesson) -> Result<()> {
        self.db.execute(
            &format!("INSERT OR IGNORE INTO lessons({COLUMNS}) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)"),
            params![
                l.id,
                l.scope,
                l.kind,
                l.text,
                l.source,
                serde_json::to_string(&l.anchors)?,
                l.helpful,
                l.harmful,
                l.credited_at
            ],
        )?;
        Ok(())
    }
    /// Deletes and tombstones `scope`'s retired lessons; how many went.
    fn retire(&mut self, scope: &str, level: Level) -> Result<usize> {
        let cutoff = now().saturating_sub(level.expiry_days() * 86_400);
        let below = level.retire_below();
        let tx = self.db.transaction()?;
        tx.execute(
            &format!("INSERT OR IGNORE INTO deleted SELECT id, scope FROM lessons WHERE scope=?1 AND ({RETIRED})"),
            params![scope, below, cutoff],
        )?;
        let n =
            tx.execute(&format!("DELETE FROM lessons WHERE scope=?1 AND ({RETIRED})"), params![scope, below, cutoff])?;
        tx.commit()?;
        Ok(n)
    }
    /// Makes room for one lesson: drops the least trusted lesson nobody
    /// vouched for. Eviction is about space, so it leaves no tombstone.
    fn make_room(&self) -> Result<()> {
        let n: i64 = self.db.query_row("SELECT count(*) FROM lessons", [], |r| r.get(0))?;
        if n < LESSONS as i64 {
            return Ok(());
        }
        let evicted = self.db.execute(
            "DELETE FROM lessons WHERE id = (SELECT id FROM lessons WHERE source != 'user'
               ORDER BY (1.0 + helpful) / (2.0 + helpful + harmful), credited_at LIMIT 1)",
            [],
        )?;
        if evicted == 0 {
            bail!("memory is full of lessons you saved; forget some first");
        }
        Ok(())
    }
}

struct Engine {
    store: Store,
    boards: HashMap<String, Board>,
    /// The last lesson offered to each session, for `/memory helpful|harmful`.
    used: HashMap<String, String>,
    /// Lessons offered to each session this turn.
    delivered: HashMap<String, HashSet<String>>,
    /// Each scope's ranked lessons (not preferences) and their index.
    cache: HashMap<String, (Vec<Lesson>, crate::recall::Index)>,
    blobs: Blobs,
}
impl Engine {
    fn new(home: &Path) -> Result<Self> {
        Ok(Self {
            store: Store::open(home)?,
            boards: HashMap::new(),
            used: HashMap::new(),
            delivered: HashMap::new(),
            cache: HashMap::new(),
            blobs: Blobs::default(),
        })
    }
    /// Prunes a scope and caches its lessons with their index. Anchored paths
    /// are indexed twice: a lesson is most about its files.
    fn load(&mut self, scope: &str, level: Level) -> Result<()> {
        if self.cache.contains_key(scope) {
            return Ok(());
        }
        self.store.retire(scope, level)?;
        if self.cache.len() >= SESSIONS {
            self.cache.clear();
        }
        let lessons: Vec<Lesson> = self.store.lessons(scope)?.into_iter().filter(|l| l.kind != "preference").collect();
        let docs: Vec<Vec<String>> = lessons
            .iter()
            .map(|l| {
                let paths: String = l.anchors.iter().map(|a| format!(" {0} {0}", a.path)).collect();
                crate::recall::terms(&format!("{}{paths}", l.text))
            })
            .collect();
        let index = crate::recall::Index::new(&docs);
        self.cache.insert(scope.to_owned(), (lessons, index));
        Ok(())
    }
    /// The project's and global preferences, as prompt lines.
    fn preferences(&self, scope: &str) -> Result<String> {
        let mut q = self.store.db.prepare_cached(
            "SELECT text FROM lessons WHERE kind='preference' AND scope IN (?1, ?2) ORDER BY credited_at",
        )?;
        let texts = q.query_map([scope, GLOBAL], |r| r.get::<_, String>(0))?;
        let mut out = String::new();
        for t in texts {
            let line = format!("- {}\n", t?);
            if out.len() + line.len() > 1200 {
                break;
            }
            out.push_str(&line);
        }
        Ok(out)
    }
    /// Adds `weight` outcomes to each lesson, then prunes the scope. Only a
    /// success restarts the expiry clock.
    fn credit(&mut self, scope: &str, ids: &[String], helpful: bool, weight: u64, level: Level) -> Result<usize> {
        let (column, clock) = if helpful { ("helpful", ", credited_at=?2") } else { ("harmful", "") };
        let tx = self.store.db.transaction()?;
        for id in ids {
            tx.execute(
                &format!("UPDATE lessons SET {column}={column}+?1{clock} WHERE id=?3 AND scope=?4"),
                params![weight, now(), id, scope],
            )?;
        }
        tx.commit()?;
        self.cache.remove(scope);
        self.store.retire(scope, level)
    }
    fn remember(&mut self, r: &Request) -> Result<Response> {
        let text = clip(r.data["text"].as_str().unwrap_or("").trim(), 1000);
        let kind = r.data["kind"].as_str().unwrap_or("fact");
        let source = r.data["source"].as_str().unwrap_or("user");
        let global = r.data["global"].as_bool() == Some(true);
        if text.is_empty() {
            bail!("empty lesson");
        }
        if !KINDS.contains(&kind) {
            bail!("unknown memory kind");
        }
        if !SOURCES.contains(&source) {
            bail!("unknown memory source");
        }
        if global && kind != "preference" {
            bail!("only preferences can be global");
        }
        let scope = if global { GLOBAL } else { r.scope.as_str() };
        let root = r.data["root"].as_str().unwrap_or("");
        let anchors: Vec<Anchor> = if r.level.anchors() && !root.is_empty() {
            let paths = r.data["files"].as_array().into_iter().flatten().filter_map(Value::as_str).take(4);
            paths.filter_map(|p| Some(Anchor { path: p.into(), blob: self.blobs.get(Path::new(root), p)? })).collect()
        } else {
            Vec::new()
        };
        let id = lesson_id(scope, kind, &text);
        if self.store.tombstoned(&id)? {
            bail!("this lesson was forgotten; save revised text instead");
        }
        // A near-duplicate is the same lesson: refresh it instead of adding a
        // copy that would split its outcomes.
        // The new lesson's side is worked out once, not once per stored lesson.
        let (negations, terms) = (polarity(&text), duplicate_terms(&text));
        let twin = self.store.lessons(scope)?.into_iter().find(|l| {
            l.kind == kind
                && (l.id == id
                    || (polarity(&l.text) == negations && similarity(&duplicate_terms(&l.text), &terms) >= DUPLICATE))
        });
        let (prefix, reply_id) = match twin {
            Some(l) => {
                self.store.db.execute(
                    "UPDATE lessons SET anchors=CASE WHEN ?1='[]' THEN anchors ELSE ?1 END,
                       source=CASE WHEN ?2='user' THEN 'user' ELSE source END WHERE id=?3",
                    params![serde_json::to_string(&anchors)?, source, l.id],
                )?;
                (KNOWN_PREFIX, l.id)
            }
            None => {
                self.store.make_room()?;
                let l = Lesson {
                    id: id.clone(),
                    scope: scope.into(),
                    kind: kind.into(),
                    text,
                    source: source.into(),
                    anchors,
                    helpful: 0,
                    harmful: 0,
                    credited_at: now(),
                };
                self.store.insert(&l)?;
                (SAVED_PREFIX, id)
            }
        };
        self.cache.clear();
        Ok(Response { text: format!("{prefix}{reply_id}"), id: reply_id, ..Response::default() })
    }
    /// The best lesson for this session right now, if any clears the bar.
    fn advise(&mut self, r: &Request, key: &str) -> Result<Response> {
        let mut reply = Response { seq: r.seq, ..Response::default() };
        let Some(b) = self.boards.get(key) else { return Ok(reply) };
        if b.seq != r.seq || b.complete || self.delivered.get(key).is_some_and(|d| d.len() >= r.level.per_turn()) {
            return Ok(reply);
        }
        let query = query(b, r.level.observes());
        let root = b.root.clone();
        self.load(&r.scope, r.level)?;
        let (lessons, index) = &self.cache[&r.scope];
        let done = self.delivered.get(key);
        let threshold = r.level.threshold();
        let mut ranked: Vec<(f64, usize)> = index
            .scores(&query)
            .into_iter()
            .enumerate()
            .filter(|(i, s)| *s > 0.0 && !done.is_some_and(|d| d.contains(&lessons[*i].id)))
            .map(|(i, s)| (s / (s + RELEVANCE_HALF) * lessons[i].confidence(), i))
            .filter(|(score, _)| *score >= threshold)
            .collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        // Files are checked only for the few best, inside the hook deadline.
        let mut best: Option<(f64, usize, Vec<String>)> = None;
        for (score, i) in ranked.into_iter().take(3) {
            let (fresh, changed) =
                if r.level.anchors() { self.blobs.freshness(&root, &lessons[i].anchors) } else { (1.0, vec![]) };
            if score * fresh >= threshold && best.as_ref().is_none_or(|(old, _, _)| score * fresh > *old) {
                best = Some((score * fresh, i, changed));
            }
        }
        let Some((_, i, changed)) = best else { return Ok(reply) };
        let l = &lessons[i];
        let standing = match (l.helpful, l.source.as_str()) {
            (0, "user") => "saved by the user".to_string(),
            (0, _) => "unverified".to_string(),
            (n, _) => format!("worked {n}×"),
        };
        let stale = if changed.is_empty() {
            String::new()
        } else {
            format!(
                " Changed since: {}; read the current version first and follow it where it differs from this lesson.",
                changed.join(", ")
            )
        };
        reply.text = format!("- {} ({standing}){stale} [memory:{}]", l.text, l.id);
        reply.id = l.id.clone();
        reply.data = json!({"text": l.text});
        self.delivered.entry(key.to_owned()).or_default().insert(l.id.clone());
        self.used.insert(key.to_owned(), l.id.clone());
        Ok(reply)
    }
    fn handle(&mut self, mut r: Request) -> Result<Response> {
        if r.op != "import" && serde_json::to_vec(&r)?.len() > FRAME {
            bail!("memory frame too large");
        }
        if r.scope.len() > 128 || r.session.len() > 128 || r.scope.is_empty() || r.session.is_empty() {
            bail!("invalid memory identity");
        }
        let key = format!("{}:{}", r.scope, r.session);
        let mut reply = Response { seq: r.seq, ..Response::default() };
        if !r.level.advises() && r.op != "shutdown" {
            reply.text = "memory is off".into();
            return Ok(reply);
        }
        // Every ingress path uses the same filter, including direct CLI/RPC
        // callers. Imported canonical IDs must not be silently rewritten.
        if r.op != "import" {
            crate::privacy::scrub(&mut r.data);
        }
        match r.op.as_str() {
            "begin" | "event" | "finish" => {
                if self.boards.len() >= SESSIONS && !self.boards.contains_key(&key) {
                    if let Some(k) = self.boards.keys().next().cloned() {
                        self.boards.remove(&k);
                        self.used.remove(&k);
                        self.delivered.remove(&k);
                    }
                }
                let board = self.boards.entry(key.clone()).or_default();
                if r.seq <= board.seq {
                    bail!("stale memory event");
                }
                board.seq = r.seq;
                match r.op.as_str() {
                    "begin" => {
                        board.request = clip(r.data["text"].as_str().unwrap_or(""), 4096);
                        board.root = clip(r.data["root"].as_str().unwrap_or(""), 4096);
                        board.observations.clear();
                        board.complete = false;
                        self.used.remove(&key);
                        self.delivered.remove(&key);
                        reply.data = json!({"preferences": self.preferences(&r.scope)?});
                    }
                    "event" if r.level.observes() => {
                        board.observations.push(Observation {
                            args: clip(r.data["args"].as_str().unwrap_or(""), 1024),
                            output: clip(r.data["output"].as_str().unwrap_or(""), 2048),
                        });
                        if board.observations.len() > OBSERVATIONS {
                            board.observations.remove(0);
                        }
                    }
                    "finish" => {
                        board.complete = r.data["complete"].as_bool().unwrap_or(false);
                        // Keep only the sequence and completion so stale calls still fail.
                        board.request.clear();
                        board.observations.clear();
                    }
                    _ => {}
                }
            }
            "advice" => return self.advise(&r, &key),
            "remember" => return self.remember(&r),
            "recall" | "list" => {
                let mut lessons = self.store.lessons(&r.scope)?;
                lessons.extend(self.store.lessons(GLOBAL)?);
                let mut order: Vec<(f64, &Lesson)> = if r.op == "recall" {
                    let docs: Vec<Vec<String>> = lessons.iter().map(|l| crate::recall::terms(&l.text)).collect();
                    let q = crate::recall::query(r.data["query"].as_str().unwrap_or(""));
                    let scores = crate::recall::Index::new(&docs).scores(&q);
                    lessons.iter().zip(scores).filter(|(_, s)| *s > 0.0).map(|(l, s)| (s, l)).collect()
                } else {
                    lessons.iter().map(|l| (l.confidence(), l)).collect()
                };
                order.sort_by(|a, b| b.0.total_cmp(&a.0));
                let total = order.len();
                let cap = if r.op == "recall" { 4096 } else { 24_576 };
                let lines: Vec<String> = order
                    .iter()
                    .take(if r.op == "recall" { 10 } else { 100 })
                    .map(|(_, l)| {
                        let files =
                            if l.anchors.is_empty() { String::new() } else { format!(", {} files", l.anchors.len()) };
                        let scope = if l.scope == GLOBAL { ", global" } else { "" };
                        format!(
                            "[{}:{}] {} (confidence {:.2}, +{} -{}, {}{files}{scope})",
                            l.kind,
                            l.id,
                            l.text,
                            l.confidence(),
                            l.helpful,
                            l.harmful,
                            l.source
                        )
                    })
                    .collect();
                // An empty reply reads as "memory is empty"; say what is there.
                reply.text = if lessons.is_empty() {
                    "no memories saved yet".into()
                } else if lines.is_empty() {
                    format!("no memories match; {} saved in total", lessons.len())
                } else {
                    bounded_listing(&r.op, &lines, total, cap)
                };
            }
            "forget" => {
                let id = r.data["id"].as_str().unwrap_or("");
                let tx = self.store.db.transaction()?;
                let scope: Option<String> = tx
                    .query_row(
                        "SELECT scope FROM lessons WHERE id=?1 AND scope IN (?2, ?3)",
                        params![id, r.scope, GLOBAL],
                        |r| r.get(0),
                    )
                    .ok();
                if let Some(scope) = &scope {
                    tx.execute("DELETE FROM lessons WHERE id=?1", [id])?;
                    tx.execute("INSERT OR IGNORE INTO deleted VALUES(?1,?2)", params![id, scope])?;
                }
                tx.commit()?;
                reply.text = if scope.is_some() { format!("forgot {id}") } else { "no memory in this scope".into() };
                self.store.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
                self.used.retain(|_, last| last != id);
                self.cache.clear();
            }
            "feedback" => {
                // A person's judgement counts as two outcomes.
                let Some(id) = self.used.remove(&key) else {
                    bail!("no lesson was offered in this session to rate");
                };
                let helpful = r.data["helpful"].as_bool().context("helpful must be boolean")?;
                let retired = self.credit(&r.scope, &[id], helpful, 2, r.level)?;
                reply.text = format!("feedback recorded{}", if retired > 0 { "; the lesson was retired" } else { "" });
            }
            "credit" => {
                if !r.level.credits() {
                    bail!("outcome credit needs --memory learn or higher");
                }
                let ids: Vec<String> = r.data["ids"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .take(DELIVERED)
                    .map(str::to_owned)
                    .collect();
                let passed = match r.data["verified"].as_str() {
                    Some("pass") => true,
                    Some("fail") => false,
                    _ => bail!("verified must be pass or fail"),
                };
                let retired = self.credit(&r.scope, &ids, passed, 1, r.level)?;
                reply.text = format!("credited {} lesson(s); retired {retired}", ids.len());
                reply.data = json!({"credited": ids.len(), "retired": retired});
            }
            "export" => {
                let deleted = self
                    .store
                    .db
                    .prepare("SELECT id FROM deleted WHERE scope=?1")?
                    .query_map([&r.scope], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                reply.data = json!({"version": SCHEMA, "scope": r.scope, "lessons": self.store.lessons(&r.scope)?, "deleted": deleted});
            }
            "import" => {
                if r.data["version"] != SCHEMA || r.data["scope"] != r.scope {
                    bail!("memory export version or scope mismatch");
                }
                let lessons: Vec<Lesson> = serde_json::from_value(r.data["lessons"].clone())?;
                let deleted: Vec<String> = serde_json::from_value(r.data.get("deleted").cloned().unwrap_or(json!([])))?;
                if lessons.len() > LESSONS {
                    bail!("import too large");
                }
                for l in &lessons {
                    if l.scope != r.scope
                        || l.text.is_empty()
                        || l.text.len() > 1000
                        || crate::privacy::redact(&l.text).0 != l.text
                        || !KINDS.contains(&l.kind.as_str())
                        || !SOURCES.contains(&l.source.as_str())
                        || l.id != lesson_id(&l.scope, &l.kind, &l.text)
                        || l.anchors.len() > 4
                        || l.anchors.iter().any(|a| {
                            !safe_path(&a.path) || a.blob.len() != 40 || !a.blob.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                    {
                        bail!("invalid imported lesson");
                    }
                }
                if deleted.len() > 65536
                    || deleted.iter().any(|s| s.len() != 16 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
                {
                    bail!("invalid imported deletion");
                }
                let tx = self.store.db.transaction()?;
                for id in deleted {
                    tx.execute("DELETE FROM lessons WHERE id=?1 AND scope=?2", params![id, r.scope])?;
                    tx.execute("INSERT OR IGNORE INTO deleted VALUES(?1,?2)", params![id, r.scope])?;
                }
                tx.commit()?;
                // Outcomes don't travel: an imported lesson earns trust here.
                for l in lessons {
                    if !self.store.tombstoned(&l.id)? {
                        self.store.insert(&Lesson { helpful: 0, harmful: 0, credited_at: now(), ..l })?;
                    }
                }
                let n: i64 = self.store.db.query_row("SELECT count(*) FROM lessons", [], |r| r.get(0))?;
                if n > LESSONS as i64 {
                    bail!("memory capacity exceeded by the import");
                }
                self.cache.clear();
                reply.text = "imported scoped lessons; outcomes start fresh".into();
            }
            "shutdown" => reply.text = "memory daemon stopped".into(),
            "status" => {
                let (lessons, credited, outcomes): (i64, i64, i64) = self.store.db.query_row(
                    "SELECT count(*), count(*) FILTER (WHERE helpful+harmful>0), coalesce(sum(helpful+harmful),0)
                     FROM lessons WHERE scope=?1",
                    [&r.scope],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                let l1 = self
                    .boards
                    .get(&key)
                    .map(|b| json!({"seq":b.seq,"observations":b.observations.len(),"complete":b.complete}));
                reply.data = json!({"level": r.level, "lessons": lessons, "credited_lessons": credited, "outcomes": outcomes,
                    "sessions": self.boards.len(), "l1": l1});
                reply.text = reply.data.to_string();
            }
            _ => bail!("unknown memory operation"),
        }
        Ok(reply)
    }
}

/// What reflecting on a checked goal yields.
#[derive(Debug, PartialEq)]
pub struct Reflection {
    /// The offered lessons the agent actually relied on.
    pub used: Vec<String>,
    /// New lessons, only ever from a passing goal.
    pub lessons: Vec<NewLesson>,
}
#[derive(Debug, PartialEq)]
pub struct NewLesson {
    pub kind: String,
    pub text: String,
    pub files: Vec<String>,
}

/// The reflection request: the goal, how its check ended, the lessons that
/// were offered and the working set (files changed, commands and errors).
pub fn reflect_prompt(
    objective: &str,
    passed: bool,
    offered: &[(String, String)],
    work: &str,
    max_new: usize,
) -> String {
    let offered = if offered.is_empty() {
        "(none)".to_string()
    } else {
        offered.iter().map(|(id, text)| format!("- {id}: {}", clip(text, 400))).collect::<Vec<_>>().join("\n")
    };
    let ask = if passed {
        format!(
            "Then add at most {max_new} new lessons that would help a future session in this repository: a fix \
             pattern, a command that must be run a certain way, or a trap and how to avoid it. Each is one \
             specific sentence under 200 characters naming the files or commands involved. Add none if nothing \
             reusable was learned; never restate the task or its result."
        )
    } else {
        "The check failed, so add no new lessons.".to_string()
    };
    format!(
        "A coding goal just finished and its fixed acceptance check {}.\n\nGoal: {}\n\nLessons offered from \
         memory:\n{offered}\n\nWhat happened:\n{}\n\nSay which offered lessons the work actually relied on (by \
         id; leave out ones it ignored). {ask}\n\nReply with JSON only: {{\"used\": [\"id\"], \"lessons\": \
         [{{\"kind\": \"fact|decision|gotcha\", \"text\": \"...\", \"files\": [\"path\"]}}]}}",
        if passed { "passed" } else { "failed" },
        clip(objective, 2000),
        clip(work, 4000),
    )
}

/// Reads the model's reflection strictly: only offered ids count as used,
/// and new lessons need a pass, a known kind, one short sentence and at most
/// four plain relative paths. Anything malformed yields None.
pub fn parse_reflection(content: &str, offered: &[String], passed: bool, max_new: usize) -> Option<Reflection> {
    let v: Value = serde_json::from_str(content.get(content.find('{')?..=content.rfind('}')?)?).ok()?;
    let mut used = Vec::new();
    for id in v["used"].as_array()?.iter().filter_map(Value::as_str) {
        if offered.iter().any(|o| o == id) && !used.iter().any(|u| u == id) {
            used.push(id.to_string());
        }
    }
    let mut lessons = Vec::new();
    for l in v["lessons"].as_array().into_iter().flatten().take(8) {
        if !passed || lessons.len() == max_new {
            break;
        }
        let kind = l["kind"].as_str().unwrap_or("");
        let text = crate::privacy::redact(l["text"].as_str().unwrap_or("").trim()).0;
        if !["fact", "decision", "gotcha"].contains(&kind) || text.is_empty() || text.chars().count() > 200 {
            continue;
        }
        let files = l["files"].as_array().into_iter().flatten().filter_map(Value::as_str).filter(|f| safe_path(f));
        lessons.push(NewLesson { kind: kind.into(), text, files: files.take(4).map(str::to_owned).collect() });
    }
    Some(Reflection { used, lessons })
}

pub fn serve(home: &Path) -> Result<()> {
    let dir = home.join("memory");
    crate::privacy::private_dir(&dir)?;
    // A lock prevents a second daemon from unlinking an active socket.
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("daemon.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("memory daemon already running");
    }
    // A connectable socket must mean initialization is complete. Cold overlay
    // filesystems can take longer than the hook deadline to initialize SQLite.
    let mut engine = Engine::new(home)?;
    let path = dir.join("advisor.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        if stream.set_read_timeout(Some(Duration::from_millis(50))).is_err()
            || stream.set_write_timeout(Some(Duration::from_millis(50))).is_err()
        {
            continue;
        }
        let mut s = String::new();
        if BufReader::new((&stream).take(TRANSFER as u64)).read_line(&mut s).is_err() || !s.ends_with('\n') {
            continue;
        }
        let shutdown = serde_json::from_str::<Request>(&s).is_ok_and(|r| r.op == "shutdown");
        let response = serde_json::from_str::<Request>(&s)
            .map_err(anyhow::Error::from)
            .and_then(|r| engine.handle(r))
            .unwrap_or_else(|e| Response { text: e.to_string(), error: true, ..Response::default() });
        if let Ok(mut body) = serde_json::to_vec(&response) {
            if body.len() > TRANSFER {
                body = serde_json::to_vec(&Response {
                    text: "memory response exceeds frame limit".into(),
                    error: true,
                    ..Response::default()
                })?;
            }
            body.push(b'\n');
            let _ = stream.write_all(&body);
        }
        if shutdown && !response.error {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reworded_short_fact_counts_as_a_twin() {
        let a = "The repository's test command is `make check-all`.";
        let b = "The project's test command is `make check-all`.";
        assert!(overlap(a, b) >= DUPLICATE, "{}", overlap(a, b));
        assert!(overlap("run cargo test before pushing", "the deploy needs an aws profile") < DUPLICATE);
        assert!(overlap("run cargo test", "run cargo test --release") < DUPLICATE);
        assert!(overlap("deploy production", "never deploy production") < DUPLICATE);
        assert!(overlap("deploy production", "do not deploy production") < DUPLICATE);
    }

    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn home() -> PathBuf {
        std::env::temp_dir().join(format!(
            "rusty-memory-unit-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }
    fn engine() -> (Engine, PathBuf) {
        let p = home();
        (Engine::new(&p).unwrap(), p)
    }
    fn req(op: &str, scope: &str, seq: u64, data: Value) -> Request {
        Request { op: op.into(), scope: scope.into(), session: "test".into(), seq, level: Level::Learn, data }
    }
    fn save(e: &mut Engine, kind: &str, text: &str, source: &str) -> String {
        e.handle(req("remember", "a", 0, json!({"kind":kind,"text":text,"source":source}))).unwrap().id
    }
    fn lesson(e: &Engine, id: &str) -> Option<Lesson> {
        e.store.lessons("a").unwrap().into_iter().find(|l| l.id == id)
    }
    /// Starts a fresh session on `request` at `level` and returns its advice.
    fn ask(e: &mut Engine, session: &str, level: Level, request: &str, root: &str) -> Response {
        let mut r = req("begin", "a", 1, json!({"text":request,"root":root}));
        r.session = session.into();
        r.level = level;
        e.handle(r.clone()).unwrap();
        r.op = "advice".into();
        e.handle(r).unwrap()
    }
    fn stored(source: &str, helpful: u64, harmful: u64) -> Lesson {
        Lesson {
            id: format!("{source}{helpful}{harmful}"),
            scope: "a".into(),
            kind: "fact".into(),
            text: "t".into(),
            source: source.into(),
            anchors: Vec::new(),
            helpful,
            harmful,
            credited_at: now(),
        }
    }

    #[test]
    fn levels_build_on_each_other() {
        let all = [Level::Off, Level::Recall, Level::Learn, Level::Reflect, Level::Deep];
        for l in all {
            assert_eq!(Level::parse(l.name()), Some(l));
        }
        assert_eq!((Level::parse("v2"), Level::parse("legacy")), (None, None));
        assert_eq!(Level::default(), Level::Learn);
        assert!(!Level::Off.advises() && Level::Recall.advises());
        assert!(!Level::Recall.observes() && !Level::Recall.credits());
        assert!(
            Level::Learn.observes() && Level::Learn.anchors() && Level::Learn.credits() && !Level::Learn.reflects()
        );
        assert!(Level::Reflect.reflects() && Level::Deep.reflects());
        // Higher levels offer more readily, keep more and prune harder.
        assert!(
            Level::Recall.threshold() > Level::Learn.threshold() && Level::Learn.threshold() > Level::Deep.threshold()
        );
        assert!(Level::Recall.per_turn() < Level::Deep.per_turn());
        assert!(Level::Learn.retire_below() < Level::Deep.retire_below());
        assert!(Level::Learn.expiry_days() > Level::Deep.expiry_days());
        assert_eq!((Level::Learn.new_lessons(), Level::Reflect.new_lessons(), Level::Deep.new_lessons()), (0, 2, 3));
    }

    #[test]
    fn confidence_rewards_verified_use() {
        let fresh = stored("agent", 0, 0).confidence();
        assert!((fresh - 0.356).abs() < 0.01);
        assert!(stored("agent", 1, 0).confidence() > fresh && stored("agent", 0, 1).confidence() < fresh);
        // Many successes beat one: the estimate is cautious about small counts.
        assert!(stored("agent", 9, 1).confidence() > stored("agent", 1, 0).confidence());
        assert!(stored("user", 0, 0).confidence() > fresh);
    }

    #[test]
    fn retirement_follows_the_level() {
        let (mut e, p) = engine();
        let old = now() - 20 * 86_400;
        for (id, source, h, x, at) in [
            ("fails", "agent", 0, 2, now()),
            ("mixed", "agent", 1, 1, now()),
            ("user-fails", "user", 0, 2, now()),
            ("user-fails-a-lot", "user", 0, 8, now()),
            ("unused-20-days", "agent", 0, 0, old),
            ("user-unused", "user", 0, 0, 1),
        ] {
            let l = Lesson {
                id: id.into(),
                source: source.into(),
                helpful: h,
                harmful: x,
                credited_at: at,
                ..stored("", 0, 0)
            };
            e.store.insert(&l).unwrap();
        }
        let left = |e: &Engine| {
            let mut ids: Vec<String> = e.store.lessons("a").unwrap().into_iter().map(|l| l.id).collect();
            ids.sort();
            ids
        };
        assert_eq!(e.store.retire("a", Level::Learn).unwrap(), 2);
        assert_eq!(left(&e), ["mixed", "unused-20-days", "user-fails", "user-unused"]);
        // Deep expires after 14 days and retires below a mean of 0.4.
        e.store.retire("a", Level::Deep).unwrap();
        assert_eq!(left(&e), ["mixed", "user-fails", "user-unused"]);
        assert!(e.store.tombstoned("fails").unwrap() && e.store.tombstoned("unused-20-days").unwrap());
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn preferences_reach_every_turn_without_crowding_out_lessons() {
        let (mut e, p) = engine();
        save(&mut e, "preference", "answer in British English", "user");
        e.handle(req("remember", "a", 0, json!({"kind":"preference","text":"prefer tabs","global":true}))).unwrap();
        assert!(e.handle(req("remember", "a", 0, json!({"kind":"fact","text":"x","global":true}))).is_err());
        let id = save(&mut e, "gotcha", "run make check, not pytest, so fixtures regenerate", "user");
        let begin = e.handle(req("begin", "a", 1, json!({"text":"run the tests"}))).unwrap();
        assert_eq!(begin.data["preferences"], "- answer in British English\n- prefer tabs\n");
        // A global preference reaches other projects too.
        let other = e.handle(req("begin", "b", 1, json!({"text":"hi"}))).unwrap();
        assert_eq!(other.data["preferences"], "- prefer tabs\n");
        assert_eq!(e.handle(req("advice", "a", 1, Value::Null)).unwrap().id, id);
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn listings_report_count_limits_and_search_guidance() {
        let (mut e, p) = engine();
        for i in 0..105 {
            save(&mut e, "fact", &format!("entry marker_{i:03}"), "user");
        }
        for i in 0..12 {
            save(&mut e, "fact", &format!("needle group_{i:03} unique_{i:03}"), "user");
        }

        let list = e.handle(req("list", "a", 0, Value::Null)).unwrap().text;
        assert!(list.starts_with("Showing 100 of 117 saved entries."), "{list}");
        assert!(list.contains("use a search query to narrow it."), "{list}");
        assert_eq!(list.lines().skip(1).count(), 100, "{list}");

        let search = e.handle(req("recall", "a", 0, json!({"query":"needle"}))).unwrap().text;
        assert!(search.starts_with("Showing 10 of 12 matching saved entries."), "{search}");
        assert!(search.contains("narrower search query"), "{search}");
        assert_eq!(search.lines().skip(1).count(), 10, "{search}");
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn byte_capped_listing_omits_only_whole_entries() {
        let (mut e, p) = engine();
        for i in 0..30 {
            let text = format!("nearcap_{i:03} {} marker_{i:03}", "x".repeat(900));
            save(&mut e, "fact", &text, "user");
        }
        let list = e.handle(req("list", "a", 0, Value::Null)).unwrap().text;
        assert!(list.len() <= 24_576, "{} bytes", list.len());
        let header = list.lines().next().unwrap();
        assert!(header.starts_with("Showing ") && header.contains(" of 30 saved entries."), "{header}");
        assert!(header.contains("omitted to keep this list bounded"), "{header}");
        let shown = header
            .strip_prefix("Showing ")
            .and_then(|s| s.split_once(" of "))
            .and_then(|(n, _)| n.parse::<usize>().ok())
            .unwrap();
        let entries: Vec<_> = list.lines().skip(1).collect();
        assert_eq!(entries.len(), shown);
        assert!(shown < 30, "the fixture should exceed the byte cap");
        for entry in entries {
            let end = entry.split(" (confidence").next().expect("complete entry metadata");
            let marker = end.rsplit_once(" marker_").expect("complete lesson text").1;
            assert_eq!(marker.len(), 3, "partial final entry: {entry}");
            assert!(marker.bytes().all(|b| b.is_ascii_digit()), "{entry}");
        }
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn observations_steer_recall_from_learn_up() {
        let (mut e, p) = engine();
        save(&mut e, "gotcha", "gen_fixtures.py must run before pytest or fixtures are stale", "agent");
        let mut run = |level: Level, session: &str| {
            let before = ask(&mut e, session, level, "add a median function and test it", "");
            let mut r = req(
                "event",
                "a",
                2,
                json!({"tool":"bash","args":"pytest -q","output":"exit code: 1\nE   FileNotFoundError: fixtures/data.json"}),
            );
            r.session = session.into();
            r.level = level;
            e.handle(r.clone()).unwrap();
            r.op = "advice".into();
            (before.text, e.handle(r).unwrap())
        };
        let (before, after) = run(Level::Learn, "one");
        assert!(before.is_empty(), "nothing in the request points at the lesson");
        assert!(after.text.contains("gen_fixtures.py") && after.text.contains("(unverified)"), "{}", after.text);
        // The client gets the bare lesson, for crediting and reflection.
        assert_eq!(after.data["text"], "gen_fixtures.py must run before pytest or fixtures are stale");
        assert!(run(Level::Recall, "two").1.text.is_empty());
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn being_shown_never_changes_a_lesson() {
        let (mut e, p) = engine();
        let id = save(&mut e, "fact", "deploy scripts live in ops/deploy", "user");
        let before = lesson(&e, &id).unwrap();
        for n in 0..20 {
            assert_eq!(ask(&mut e, &format!("s{n}"), Level::Learn, "run the deploy scripts in ops", "").id, id);
        }
        assert_eq!(lesson(&e, &id).unwrap(), before);
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn verified_outcomes_credit_and_retire_lessons() {
        let (mut e, p) = engine();
        let good = save(&mut e, "gotcha", "run make check, not pytest, so fixtures regenerate", "agent");
        let bad = save(&mut e, "gotcha", "the build needs python2 everywhere", "agent");
        let credit = |e: &mut Engine, id: &str, verdict: &str| {
            e.handle(req("credit", "a", 0, json!({"ids":[id],"verified":verdict}))).unwrap().data
        };
        assert_eq!(credit(&mut e, &good, "pass")["retired"], 0);
        assert_eq!(lesson(&e, &good).unwrap().helpful, 1);
        credit(&mut e, &bad, "fail");
        assert_eq!(credit(&mut e, &bad, "fail")["retired"], 1);
        assert!(lesson(&e, &bad).is_none());
        // Retired means tombstoned: saving it again is refused.
        assert!(e
            .handle(req("remember", "a", 0, json!({"kind":"gotcha","text":"the build needs python2 everywhere"})))
            .is_err());
        // A failure doesn't restart the expiry clock: this old lesson goes at
        // its first failure instead of living another 28 days.
        let stale = save(&mut e, "fact", "the frontend lives in web/legacy", "agent");
        e.store.db.execute("UPDATE lessons SET credited_at=1 WHERE id=?1", [&stale]).unwrap();
        assert_eq!(credit(&mut e, &stale, "fail")["retired"], 1);
        assert!(e.handle(req("credit", "a", 0, json!({"ids":[&good],"verified":"maybe"}))).is_err());
        let mut recall = req("credit", "a", 0, json!({"ids":[&good],"verified":"pass"}));
        recall.level = Level::Recall;
        assert!(e.handle(recall).is_err());
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn near_duplicates_merge_and_a_full_store_evicts() {
        let (mut e, p) = engine();
        let a = save(&mut e, "gotcha", "run make check before pushing so fixtures regenerate", "agent");
        let b = save(&mut e, "gotcha", "Run make check before pushing, so the fixtures regenerate", "user");
        assert_eq!(a, b, "a paraphrase is the same lesson");
        assert_eq!(lesson(&e, &a).unwrap().source, "user", "the user vouching upgrades it");
        assert_ne!(save(&mut e, "fact", "run make check before pushing so fixtures regenerate", "agent"), a);
        // Fill the store; the next save evicts the least trusted agent lesson.
        for i in 0..LESSONS - 3 {
            e.store
                .insert(&Lesson { id: format!("filler{i}"), credited_at: now() - 1, ..stored("agent", 1, 0) })
                .unwrap();
        }
        e.store.insert(&Lesson { id: "weak".into(), ..stored("agent", 0, 1) }).unwrap();
        save(&mut e, "fact", "the api listens on port 8443", "agent");
        assert!(lesson(&e, "weak").is_none() && !e.store.tombstoned("weak").unwrap());
        // When everything left is the user's, nothing is evicted.
        e.store.db.execute("UPDATE lessons SET source='user'", []).unwrap();
        assert!(e.handle(req("remember", "a", 0, json!({"kind":"fact","text":"one more fact"}))).is_err());
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn anchors_flag_changed_files_and_hide_lessons_about_missing_ones() {
        let (mut e, p) = engine();
        let root = p.join("work");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/tax.rs"), "hello\n").unwrap();
        // The same id `git hash-object` prints.
        assert_eq!(e.blobs.get(&root, "src/tax.rs").unwrap(), "ce013625030ba8dba906f756967f9e9ca394464a");
        assert!(e.blobs.get(&root, "../outside").is_none() && e.blobs.get(&root, "/etc/passwd").is_none());
        std::os::unix::fs::symlink("/etc/hostname", root.join("link")).unwrap();
        assert!(e.blobs.get(&root, "link").is_none());
        let root_s = root.to_string_lossy().to_string();
        let data =
            json!({"kind":"fact","text":"tax rounding is half-even","files":["src/tax.rs","missing.rs"],"root":root_s});
        let id = e.handle(req("remember", "a", 0, data)).unwrap().id;
        assert_eq!(lesson(&e, &id).unwrap().anchors.len(), 1, "files that don't exist aren't anchored");
        assert!(!ask(&mut e, "s1", Level::Learn, "fix tax rounding", &root_s).text.contains("Changed since"));
        std::fs::write(root.join("src/tax.rs"), "changed\n").unwrap();
        let text = ask(&mut e, "s2", Level::Learn, "fix tax rounding", &root_s).text;
        // The wording was adopted under a predeclared test (docs/VERIFICATION.md);
        // changing it is a new test, not an edit.
        let warning = "Changed since: src/tax.rs; read the current version first and follow it where it differs from this lesson.";
        assert!(text.contains(warning), "{text}");
        // Recall doesn't check files.
        assert!(!ask(&mut e, "s3", Level::Recall, "fix tax rounding", &root_s).text.contains("Changed since"));
        std::fs::remove_file(root.join("src/tax.rs")).unwrap();
        assert!(ask(&mut e, "s4", Level::Learn, "fix tax rounding", &root_s).text.is_empty());
        assert!(lesson(&e, &id).is_some(), "another branch may still have the file");
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn freshness_by_source_unchanged_changed_and_missing_anchors() {
        // A lesson the agent saved starts below one the user vouched for, so
        // the 0.5 freshness of a changed file hides it from the request alone
        // at learn; the session's errors can still raise it later
        // (docs/MEMORY.md). A missing file hides either. Both sources are
        // covered so a change to freshness has to say what it does to each.
        for (source, offered_when_changed) in [("user", true), ("agent", false)] {
            let (mut e, p) = engine();
            let root = p.join("work");
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/tax.rs"), "hello\n").unwrap();
            let root_s = root.to_string_lossy().to_string();
            let data = json!({"kind":"fact","text":"tax rounding is half-even","source":source,
                "files":["src/tax.rs"],"root":root_s});
            e.handle(req("remember", "a", 0, data)).unwrap();
            let unchanged = ask(&mut e, "s1", Level::Learn, "fix tax rounding", &root_s).text;
            assert!(unchanged.contains("half-even") && !unchanged.contains("Changed since"), "{source}: {unchanged}");
            std::fs::write(root.join("src/tax.rs"), "changed\n").unwrap();
            let changed = ask(&mut e, "s2", Level::Learn, "fix tax rounding", &root_s).text;
            assert_eq!(changed.contains("Changed since: src/tax.rs"), offered_when_changed, "{source}: {changed}");
            std::fs::remove_file(root.join("src/tax.rs")).unwrap();
            assert!(ask(&mut e, "s3", Level::Learn, "fix tax rounding", &root_s).text.is_empty(), "{source}");
            std::fs::remove_dir_all(p).unwrap();
        }
    }

    #[test]
    fn the_store_runs_in_wal_with_normal_sync() {
        // NORMAL is only crash-safe in WAL mode; both must hold together.
        let (e, p) = engine();
        let journal: String = e.store.db.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        let sync: i64 = e.store.db.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        assert_eq!((journal.as_str(), sync), ("wal", 1), "WAL with synchronous=NORMAL");
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn unchanged_files_are_not_rehashed() {
        let p = home();
        std::fs::create_dir_all(&p).unwrap();
        let file = p.join("a.txt");
        std::fs::write(&file, "first\n").unwrap();
        let mut blobs = Blobs::default();
        let first = blobs.get(&p, "a.txt").unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();
        // Same length and mtime: the cached id is reused without reading.
        std::fs::write(&file, "other\n").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        assert_eq!(blobs.get(&p, "a.txt").unwrap(), first);
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(mtime + Duration::from_secs(5)).unwrap();
        assert_ne!(blobs.get(&p, "a.txt").unwrap(), first);
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn sessions_are_ordered_scoped_and_bounded_in_ram() {
        let (mut e, p) = engine();
        save(&mut e, "fact", "inspect the workspace manifest before editing packages", "user");
        e.handle(req("begin", "a", 1, json!({"text":"edit workspace packages"}))).unwrap();
        assert!(!e.handle(req("advice", "a", 1, Value::Null)).unwrap().text.is_empty());
        assert!(e.handle(req("advice", "b", 1, Value::Null)).unwrap().text.is_empty());
        assert!(e.handle(req("begin", "a", 1, json!({"text":"stale"}))).is_err());
        e.handle(req("finish", "a", 2, json!({"complete":true}))).unwrap();
        assert!(e.handle(req("advice", "a", 2, Value::Null)).unwrap().text.is_empty());
        let bytes =
            || std::fs::read_dir(p.join("memory")).unwrap().map(|f| f.unwrap().metadata().unwrap().len()).sum::<u64>();
        let before = bytes();
        for n in 0..120 {
            let mut r = req("begin", "a", 1, json!({"text":"workspace manifest"}));
            r.session = format!("session-{n}");
            e.handle(r.clone()).unwrap();
            r.op = "advice".into();
            e.handle(r.clone()).unwrap();
            r.op = "event".into();
            r.data = json!({"tool":"bash","args":"a".repeat(4000),"output":"b".repeat(4000)});
            for seq in 2..12 {
                r.seq = seq;
                e.handle(r.clone()).unwrap();
            }
        }
        assert_eq!(e.boards.len(), SESSIONS);
        assert!(e.used.len() <= SESSIONS && e.delivered.len() <= SESSIONS);
        assert!(e.boards.values().all(|b| b.observations.len() <= OBSERVATIONS));
        assert_eq!(e.boards.values().next().unwrap().observations[0].args.len(), 1024);
        assert_eq!(bytes(), before, "coding hooks wrote persistent data");
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn export_import_and_forget_round_trip() {
        let (mut e, p) = engine();
        let id = save(&mut e, "fact", "monorepo URL is https://github.com/example/workspace", "user");
        e.handle(req("credit", "a", 0, json!({"ids":[&id],"verified":"pass"}))).unwrap();
        let exported = e.handle(req("export", "a", 0, Value::Null)).unwrap().data;
        e.handle(req("forget", "a", 0, json!({"id":id}))).unwrap();
        e.handle(req("import", "a", 0, exported.clone())).unwrap();
        assert!(lesson(&e, &id).is_none(), "a forgotten lesson came back");
        let (mut fresh, q) = engine();
        fresh.handle(req("import", "a", 0, exported)).unwrap();
        let l = lesson(&fresh, &id).unwrap();
        assert_eq!((l.helpful, l.source.as_str()), (0, "user"), "outcomes don't travel");
        assert!(fresh.handle(req("import", "a", 0, json!({"version":1,"scope":"a","lessons":[]}))).is_err());
        std::fs::remove_dir_all(p).unwrap();
        std::fs::remove_dir_all(q).unwrap();
    }

    #[test]
    fn older_stores_are_replaced() {
        let p = home();
        crate::privacy::private_dir(&p.join("memory")).unwrap();
        {
            let db = Connection::open(p.join("memory/memory.sqlite")).unwrap();
            db.execute_batch("CREATE TABLE lessons(id TEXT, scope TEXT, kind TEXT, text TEXT, evidence BLOB, helpful INTEGER, harmful INTEGER);
                              INSERT INTO lessons VALUES('x','a','fact','old',x'00',0,0); PRAGMA user_version=2;").unwrap();
        }
        for _ in 0..2 {
            let store = Store::open(&p).unwrap();
            assert!(store.lessons("a").unwrap().is_empty());
        }
        std::fs::remove_dir_all(p).unwrap();
    }

    #[test]
    fn reflection_is_parsed_strictly() {
        let offered = vec!["aaa".to_string(), "bbb".to_string()];
        let reply = r#"Sure: {"used":["aaa","zzz","aaa"],"lessons":[
            {"kind":"gotcha","text":"run make check; pytest alone skips fixture generation","files":["Makefile","../etc"]},
            {"kind":"preference","text":"the user likes tabs"},
            {"kind":"fact","text":"x"},
            {"kind":"fact","text":"a third lesson"}]}"#;
        let r = parse_reflection(reply, &offered, true, 2).unwrap();
        assert_eq!(r.used, ["aaa"]);
        assert_eq!(r.lessons.len(), 2, "at most two, and invented preferences are dropped");
        assert_eq!(r.lessons[0].files, ["Makefile"]);
        assert_eq!(parse_reflection(reply, &offered, true, 3).unwrap().lessons.len(), 3);
        assert!(parse_reflection(reply, &offered, false, 2).unwrap().lessons.is_empty());
        assert!(parse_reflection("no json here", &offered, true, 2).is_none());
        let long = format!(r#"{{"used":[],"lessons":[{{"kind":"fact","text":"{}"}}]}}"#, "x".repeat(201));
        assert!(parse_reflection(&long, &offered, true, 2).unwrap().lessons.is_empty());
        let prompt =
            reflect_prompt("add median", true, &[("aaa".into(), "use make check".into())], "- changed: stats.py", 2);
        assert!(prompt.contains("passed") && prompt.contains("aaa: use make check") && prompt.contains("at most 2"));
        assert!(reflect_prompt("g", false, &[], "", 2).contains("add no new lessons"));
    }
}
