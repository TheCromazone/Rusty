//! Offline stand-ins for Daytona: a tiny HTTP server on std::net, and a fake
//! Daytona API (control plane, toolbox and object storage) whose "sandbox"
//! runs commands on this machine in a scratch home directory.
#![allow(dead_code)]

pub mod sync;

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const RUSTY: &str = env!("CARGO_BIN_EXE_rusty");
pub const MEMORYD: &str = env!("CARGO_BIN_EXE_rusty-memoryd");
pub const CLOUD: &str = env!("CARGO_BIN_EXE_rusty-cloud");
pub const DAYTONA_KEY: &str = "test-daytona-key-0123456789";

// ------------------------------------------------------------------ temp

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        // macOS's per-user temp path plus the stand-in home exceeds the
        // Unix socket limit. Use a short root for these offline fixtures.
        let parent = if cfg!(target_os = "macos") { PathBuf::from("/tmp") } else { std::env::temp_dir() };
        let dir =
            parent.join(format!("rusty-cloud-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir.canonicalize().unwrap())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // rusty starts a memory daemon on demand; don't leave it running.
        for home in [self.0.join("home"), self.0.clone()] {
            if home.join("memory/advisor.sock").exists() {
                let _ = Command::new(MEMORYD).arg("--home").arg(&home).arg("stop").stdout(Stdio::null()).status();
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn write_exec(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A git repository with one commit of `files`.
pub fn git_repo(dir: &Path, files: &[(&str, &str)]) {
    std::fs::create_dir_all(dir).unwrap();
    for (name, text) in files {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    for args in
        [&["init", "-q"][..], &["add", "-A"], &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"]]
    {
        let s = Command::new("git").args(args).current_dir(dir).status().unwrap();
        assert!(s.success());
    }
}

/// Outer process watchdogs are separate from the command deadline being tested.
pub fn wait(child: Child, limit: Duration) -> Output {
    wait_command(child, limit.max(Duration::from_secs(120)))
}

pub fn wait_command(child: Child, limit: Duration) -> Output {
    sync::wait(child, limit)
}

/// Splits a POSIX shell command line into words (quotes and backslashes).
pub fn split(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    cur.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => match chars.peek() {
                            Some('"' | '\\' | '$' | '`') => cur.push(chars.next().unwrap()),
                            Some('\n') => {
                                chars.next();
                            }
                            _ => cur.push('\\'),
                        },
                        c => cur.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(n) = chars.next() {
                    if n != '\n' {
                        cur.push(n);
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                cur.push(c);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    words
}

// ------------------------------------------------------------------ http

pub struct Request {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("")
    }

    pub fn query(&self, key: &str) -> Option<String> {
        let q = self.target.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == key).then(|| decode(v))
        })
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else if b[i] == b'+' {
            out.push(b' ');
            i += 1;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

pub struct Response {
    pub status: u16,
    pub kind: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, v: Value) -> Self {
        Self { status, kind: "application/json", body: v.to_string().into_bytes() }
    }
    pub fn bytes(status: u16, body: Vec<u8>) -> Self {
        Self { status, kind: "application/octet-stream", body }
    }
    pub fn sse(text: String) -> Self {
        Self { status: 200, kind: "text/event-stream", body: text.into_bytes() }
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// An HTTP/1.1 server, one request per connection, one thread each.
pub struct Server {
    pub url: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let result = thread.join();
            if !std::thread::panicking() {
                result.expect("mock server panicked");
            }
        }
    }
}

pub fn serve(handler: Handler) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let thread = std::thread::spawn(move || {
        let mut workers = Vec::new();
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let handler = handler.clone();
                    workers.push(std::thread::spawn(move || {
                        stream.set_nonblocking(false).unwrap();
                        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() {
                            return;
                        }
                        let mut parts = line.split_whitespace();
                        let method = parts.next().unwrap_or("").to_string();
                        let target = parts.next().unwrap_or("").to_string();
                        let mut headers = Vec::new();
                        let mut size = 0;
                        loop {
                            let mut h = String::new();
                            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                                return;
                            }
                            let h = h.trim_end();
                            if h.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = h.split_once(':') {
                                let k = k.trim().to_ascii_lowercase();
                                if k == "content-length" {
                                    size = v.trim().parse().unwrap_or(0);
                                }
                                headers.push((k, v.trim().to_string()));
                            }
                        }
                        let mut body = vec![0; size];
                        if reader.read_exact(&mut body).is_err() {
                            return;
                        }
                        let head = method == "HEAD";
                        let r = handler(Request { method, target, headers, body });
                        let mut stream = stream;
                        let _ = write!(
                            stream,
                            "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            r.status,
                            r.kind,
                            r.body.len()
                        );
                        if !head {
                            let _ = stream.write_all(&r.body);
                        }
                        let _ = stream.flush();
                    }));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        for worker in workers {
            worker.join().expect("mock request panicked");
        }
    });
    Server { url, stop, thread: Some(thread) }
}

// --------------------------------------------------------------- machine

/// The fake sandbox's machine: a home directory, a PATH, and a record of the
/// tool requests that ran on it.
pub struct Machine {
    pub home: PathBuf,
    pub path: Mutex<String>,
    pub env: Mutex<BTreeMap<String, String>>,
    /// Makes every toolbox call fail with a message holding a fake secret.
    pub fail: AtomicBool,
    pub calls: Mutex<Vec<Value>>,
    active: AtomicUsize,
    pub peak: AtomicUsize,
    reads: (Mutex<(usize, usize)>, std::sync::Condvar),
    children: Mutex<Vec<Child>>,
}

impl Machine {
    pub fn new(home: PathBuf, path: &str) -> Arc<Self> {
        std::fs::create_dir_all(&home).unwrap();
        Arc::new(Self {
            home,
            path: Mutex::new(path.into()),
            env: Mutex::new(BTreeMap::new()),
            fail: AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            reads: (Mutex::new((0, 0)), std::sync::Condvar::new()),
            children: Mutex::new(Vec::new()),
        })
    }

    /// Hold the first n reads until all are in flight; later reads run normally.
    pub fn expect_parallel_reads(&self, n: usize) {
        *self.reads.0.lock().unwrap() = (0, n);
    }

    fn command(&self, inner: &str) -> Command {
        let mut c = Command::new("bash");
        c.arg("-c")
            .arg(inner)
            .env_clear()
            .env("PATH", self.path.lock().unwrap().clone())
            .env("HOME", &self.home)
            .envs(self.env.lock().unwrap().clone())
            .current_dir(&self.home);
        c
    }

    /// `bash -lc '<cmd>'` as the toolbox receives it → the inner command.
    fn unwrap(command: &str) -> String {
        let words = split(command);
        assert_eq!(&words[..2], ["bash", "-lc"], "toolbox commands run through bash -lc: {command}");
        assert_eq!(words.len(), 3, "the command is one quoted word: {command}");
        words[2].clone()
    }

    /// Runs a command, recording `rusty --tool-rpc` requests; concurrent
    /// `read_file` executions can wait on an explicit arrival barrier.
    pub fn exec(&self, command: &str, timeout: u64) -> (i32, String) {
        let inner = Self::unwrap(command);
        let words = split(&inner);
        let request = words
            .iter()
            .position(|w| w == "%s")
            .filter(|_| inner.contains("rusty --tool-rpc"))
            .map(|i| serde_json::from_str::<Value>(&words[i + 1]).unwrap());
        let overlap = request.as_ref().is_some_and(|r| r["op"] == "execute" && r["name"] == "read_file");
        if let Some(r) = request {
            self.calls.lock().unwrap().push(r);
        }
        if overlap {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let (lock, cv) = &self.reads;
            let mut reads = lock.lock().unwrap();
            if reads.0 < reads.1 {
                reads.0 += 1;
                cv.notify_all();
                let (_reads, timeout) = cv.wait_timeout_while(reads, Duration::from_secs(120), |r| r.0 < r.1).unwrap();
                assert!(!timeout.timed_out(), "remote reads did not overlap");
            }
        }
        let child = self.command(&inner).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let out = wait_command(child, Duration::from_secs(timeout));
        if overlap {
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), text)
    }

    pub fn start(&self, command: &str) {
        let inner = Self::unwrap(command);
        let child = self.command(&inner).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        self.children.lock().unwrap().push(child.unwrap());
    }

    /// Waits for background commands, so nothing outlives a test.
    pub fn join(&self) {
        for mut c in self.children.lock().unwrap().drain(..) {
            let start = Instant::now();
            while c.try_wait().unwrap().is_none() && start.elapsed() < Duration::from_secs(20) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        self.join();
    }
}

// --------------------------------------------------------------- daytona

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub target: String,
    pub auth: Option<String>,
    pub body: Value,
}

#[derive(Default)]
pub struct State {
    pub requests: Vec<Recorded>,
    /// name → (id, state, polls left before active)
    pub snapshots: BTreeMap<String, (String, String, u32)>,
    /// id → sandbox JSON
    pub sandboxes: BTreeMap<String, Value>,
    pub deleted_sandboxes: Vec<String>,
    pub deleted_snapshots: Vec<String>,
    pub s3: BTreeMap<String, Vec<u8>>,
    pub problems: Vec<String>,
}

pub const PUSH: (&str, &str, &str) = ("AKIDTEST", "s3-secret-value", "s3-session-token");

/// Daytona's control plane at `<url>/api`, the toolbox at `<url>/toolbox`
/// and object storage at `<url>/s3`, all backed by one `Machine`.
pub struct FakeDaytona {
    pub server: Server,
    pub url: String,
    pub state: Arc<Mutex<State>>,
    pub machine: Arc<Machine>,
}

impl FakeDaytona {
    pub fn new(machine: Arc<Machine>) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let base = Arc::new(Mutex::new(String::new()));
        let (s, m, b) = (state.clone(), machine.clone(), base.clone());
        let server = serve(Arc::new(move |req| {
            let base = b.lock().unwrap().clone();
            handle(&s, &m, &base, req)
        }));
        *base.lock().unwrap() = server.url.clone();
        Self { url: server.url.clone(), server, state, machine }
    }

    pub fn api(&self) -> String {
        format!("{}/api", self.url)
    }

    pub fn add_snapshot(&self, name: &str, state: &str) {
        self.state.lock().unwrap().snapshots.insert(name.into(), (format!("snap-{name}"), state.into(), 0));
    }

    pub fn add_sandbox(&self, id: &str, state: &str, labels: Value) {
        self.state.lock().unwrap().sandboxes.insert(id.into(), json!({"id": id, "state": state, "labels": labels}));
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.state.lock().unwrap().requests.clone()
    }

    pub fn assert_ok(&self) {
        let problems = self.state.lock().unwrap().problems.clone();
        assert!(problems.is_empty(), "fake Daytona saw problems: {problems:#?}");
    }

    /// `rusty-cloud` with only this API and the given variables.
    pub fn cloud(&self, cwd: &Path) -> Command {
        let mut c = Command::new(CLOUD);
        c.current_dir(cwd)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", cwd)
            .env("DAYTONA_API_KEY", DAYTONA_KEY)
            .env("DAYTONA_API_URL", self.api());
        c
    }
}

fn snapshot_json(name: &str, (id, state, _): &(String, String, u32)) -> Value {
    json!({"id": id, "name": name, "state": state, "errorReason": null})
}

fn handle(state: &Mutex<State>, machine: &Machine, base: &str, req: Request) -> Response {
    let path = req.path().to_string();
    let body_json = req.json();
    {
        let mut s = state.lock().unwrap();
        s.requests.push(Recorded {
            method: req.method.clone(),
            target: req.target.clone(),
            auth: req.header("authorization").map(Into::into),
            body: body_json.clone(),
        });
        let authed = req.header("authorization") == Some(&format!("Bearer {DAYTONA_KEY}"));
        if (path.starts_with("/api/") || path.starts_with("/toolbox/") || path.starts_with("/logs/")) && !authed {
            s.problems.push(format!("unauthenticated {} {}", req.method, req.target));
            return Response::json(401, json!({"message": "unauthorized"}));
        }
    }
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut s = state.lock().unwrap();
    match (req.method.as_str(), seg.as_slice()) {
        // The model endpoint reachability probe.
        ("GET", ["v1", "models"]) => Response::json(401, json!({"error": "no key"})),
        ("GET", ["api", "snapshots"]) => {
            let items: Vec<Value> = s.snapshots.iter().map(|(n, v)| snapshot_json(n, v)).collect();
            Response::json(200, json!({"items": items, "total": items.len(), "page": 1, "totalPages": 1}))
        }
        ("GET", ["api", "snapshots", key]) => {
            let key = decode(key);
            let found = s.snapshots.iter_mut().find(|(n, v)| **n == key || v.0 == key);
            match found {
                Some((name, v)) => {
                    if v.1 != "active" && v.2 > 0 {
                        v.2 -= 1;
                        v.1 = if v.2 == 0 { "active".into() } else { "building".into() };
                    }
                    Response::json(200, snapshot_json(name, v))
                }
                None => Response::json(404, json!({"message": "snapshot not found"})),
            }
        }
        ("GET", ["api", "snapshots", id, "build-logs-url"]) => {
            Response::json(200, json!({"url": format!("{base}/logs/{id}")}))
        }
        ("GET", ["logs", _]) => Response::bytes(200, b"step 1/2\nstep 2/2\n".to_vec()),
        ("POST", ["api", "snapshots"]) => {
            let name = body_json["name"].as_str().unwrap_or_default().to_string();
            for hash in body_json["buildInfo"]["contextHashes"].as_array().cloned().unwrap_or_default() {
                let key = format!("org-1/{}/context.tar", hash.as_str().unwrap_or_default());
                if !s.s3.contains_key(&key) {
                    s.problems.push(format!("snapshot names a context that was never uploaded: {key}"));
                }
            }
            let v = (format!("snap-{name}"), "pending".to_string(), 2);
            let out = snapshot_json(&name, &v);
            s.snapshots.insert(name, v);
            Response::json(200, out)
        }
        ("DELETE", ["api", "snapshots", id]) => {
            let id = decode(id);
            let name = s.snapshots.iter().find(|(_, v)| v.0 == id).map(|(n, _)| n.clone());
            match name {
                Some(n) => {
                    s.snapshots.remove(&n);
                    s.deleted_snapshots.push(id);
                    Response::json(200, json!({}))
                }
                None => Response::json(404, json!({"message": "not found"})),
            }
        }
        ("GET", ["api", "object-storage", "push-access"]) => Response::json(
            200,
            json!({"accessKey": PUSH.0, "secret": PUSH.1, "sessionToken": PUSH.2, "storageUrl": format!("{base}/s3"),
                   "organizationId": "org-1", "bucket": "daytona-volume-builds", "region": "us-east-1"}),
        ),
        (method @ ("HEAD" | "PUT"), ["s3", bucket, rest @ ..]) => {
            let key = rest.join("/");
            check_sigv4(&mut s, &req, &path);
            if *bucket != "daytona-volume-builds" {
                s.problems.push(format!("wrong bucket {bucket}"));
            }
            if method == "HEAD" {
                Response::bytes(if s.s3.contains_key(&key) { 200 } else { 404 }, Vec::new())
            } else {
                s.s3.insert(key, req.body.clone());
                Response::bytes(200, Vec::new())
            }
        }
        ("POST", ["api", "sandbox"]) => {
            let id = format!("sbx-{}", s.sandboxes.len() + 1);
            let snapshot = body_json["snapshot"].as_str().unwrap_or_default();
            if !s.snapshots.contains_key(snapshot) {
                return Response::json(400, json!({"message": "no such snapshot"}));
            }
            *machine.env.lock().unwrap() = body_json["env"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
                .unwrap_or_default();
            // Starts in "creating"; the first GET reports "started".
            let sandbox = json!({"id": id, "state": "creating", "labels": body_json["labels"]});
            s.sandboxes.insert(id.clone(), json!({"id": id, "state": "started", "labels": body_json["labels"]}));
            Response::json(200, sandbox)
        }
        ("GET", ["api", "sandbox", id]) => match s.sandboxes.get(&decode(id)) {
            Some(v) => Response::json(200, v.clone()),
            None => Response::json(404, json!({"message": "sandbox not found"})),
        },
        ("GET", ["api", "sandbox", id, "toolbox-proxy-url"]) => {
            let _ = id;
            Response::json(200, json!({"url": format!("{base}/toolbox")}))
        }
        ("DELETE", ["api", "sandbox", id]) => {
            let id = decode(id);
            if s.sandboxes.remove(&id).is_some() {
                s.deleted_sandboxes.push(id.clone());
                Response::json(200, json!({"id": id, "state": "destroying"}))
            } else {
                Response::json(404, json!({"message": "not found"}))
            }
        }
        (_, ["toolbox", id, rest @ ..]) => {
            if !s.sandboxes.contains_key(&decode(id)) {
                return Response::json(404, json!({"message": "no such sandbox"}));
            }
            if machine.fail.load(Ordering::SeqCst) {
                return Response::json(500, json!({"message": "toolbox failed for credential-canary-never-log"}));
            }
            drop(s);
            toolbox(state, machine, &req, &body_json, rest)
        }
        _ => {
            s.problems.push(format!("unexpected {} {}", req.method, req.target));
            Response::json(404, json!({"message": "unknown route"}))
        }
    }
}

fn toolbox(state: &Mutex<State>, machine: &Machine, req: &Request, body: &Value, rest: &[&str]) -> Response {
    let problem = |p: String| state.lock().unwrap().problems.push(p);
    match (req.method.as_str(), rest) {
        ("POST", ["process", "execute"]) => {
            if body.get("envs").is_some() {
                problem("the host environment was exported with a command".into());
            }
            let command = body["command"].as_str().unwrap_or_default();
            let timeout = body["timeout"].as_u64().unwrap_or(10);
            let (code, out) = machine.exec(command, timeout);
            Response::json(200, json!({"exitCode": code, "result": out}))
        }
        ("POST", ["process", "session"]) => {
            if body["sessionId"].as_str().is_none() {
                problem("session without an id".into());
            }
            Response::json(201, Value::Null)
        }
        ("POST", ["process", "session", _, "exec"]) => {
            if body["runAsync"] != true {
                problem("session command was not asynchronous".into());
            }
            machine.start(body["command"].as_str().unwrap_or_default());
            Response::json(200, json!({"cmdId": "cmd-1"}))
        }
        ("POST", ["files", "upload-v2"]) => {
            let path = req.query("path").unwrap_or_default();
            if !path.starts_with(machine.home.to_str().unwrap()) {
                problem(format!("upload outside the sandbox home: {path}"));
            }
            std::fs::write(&path, &req.body).unwrap();
            Response::json(200, json!({"path": path}))
        }
        ("GET", ["files", "download"]) => {
            let path = req.query("path").unwrap_or_default();
            match std::fs::read(&path) {
                Ok(data) => Response::bytes(200, data),
                Err(_) => Response::json(404, json!({"message": "file not found"})),
            }
        }
        _ => {
            problem(format!("unexpected toolbox {} {}", req.method, req.target));
            Response::json(404, json!({"message": "unknown toolbox route"}))
        }
    }
}

/// Recomputes the request's AWS signature from what actually arrived.
fn check_sigv4(s: &mut State, req: &Request, path: &str) {
    let h = |k: &str| req.header(k).unwrap_or_default().to_string();
    let payload = h("x-amz-content-sha256");
    if payload != rusty::cloud::digest::sha256_hex(&req.body) {
        s.problems.push("payload hash does not match the body".into());
    }
    if h("x-amz-security-token") != PUSH.2 {
        s.problems.push("missing session token".into());
    }
    let (host, date) = (h("host"), h("x-amz-date"));
    let expected = rusty::cloud::image::sigv4(
        &req.method,
        path,
        "",
        &[("host", &host), ("x-amz-content-sha256", &payload), ("x-amz-date", &date), ("x-amz-security-token", PUSH.2)],
        &payload,
        "us-east-1",
        PUSH.0,
        PUSH.1,
        &date,
    );
    if h("authorization") != expected {
        s.problems.push(format!("bad S3 signature for {} {path}", req.method));
    }
}

// ----------------------------------------------------------------- model

/// An OpenAI-compatible SSE reply with one tool call or a final text.
pub fn reply(step: usize, action: Option<(&str, Value)>, text: &str) -> Response {
    let (delta, finish) = match action {
        Some((name, args)) => (
            json!({"tool_calls": [{"index": 0, "id": format!("call-{step}"), "type": "function",
                   "function": {"name": name, "arguments": args.to_string()}}]}),
            "tool_calls",
        ),
        None => (json!({"content": text}), "stop"),
    };
    let frames = [
        json!({"choices": [{"delta": delta, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": finish}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}),
    ];
    let mut out: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect();
    out.push_str("data: [DONE]\n\n");
    Response::sse(out)
}

/// The last user prompt and the number of tool results so far.
pub fn conversation(body: &Value) -> (String, usize, &Vec<Value>) {
    let messages = body["messages"].as_array().expect("messages");
    let prompt = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default()
        .to_string();
    let steps = messages.iter().filter(|m| m["role"] == "tool").count();
    (prompt, steps, messages)
}

pub fn content(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
