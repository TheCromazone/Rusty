//! `cargo xtask bench`: wall time, peak memory and disk for the paths a user
//! waits on, offline and from fixed inputs, so two builds can be compared.
//!
//! Wall is the median of `--runs` runs after one warm-up. Peak RSS is the
//! child's `ru_maxrss` from wait4, the number `/usr/bin/time -l` prints, and
//! CPU is its user + system time: steadier than wall on a busy machine.
//! Model traffic goes to a local mock speaking the OpenAI SSE format.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

const USAGE: &str = "usage: cargo xtask bench [--binary PATH] [--json PATH] [--runs N] [--only GROUP,...]\n\
    Builds release rusty (unless --binary) and the perf example, then prints one table.\n\
    Groups: startup, sessions, redact, recall, turn, stream, size.";

pub fn command(args: &[String]) -> Result<ExitCode> {
    let (mut binary, mut out, mut runs) = (None, super::target_dir().join("bench.json"), 11usize);
    let mut only = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--binary" => binary = Some(PathBuf::from(value()?)),
            "--json" => out = PathBuf::from(value()?),
            "--runs" => runs = value()?.parse().context("--runs")?,
            "--only" => only = value()?.split(',').map(str::to_string).collect(),
            _ => {
                println!("{USAGE}");
                return Ok(ExitCode::from(if arg == "-h" || arg == "--help" { 0 } else { 2 }));
            }
        }
    }
    let binary = match binary {
        Some(b) => std::fs::canonicalize(&b).with_context(|| format!("{}", b.display()))?,
        None => {
            super::sh("cargo", &["build", "--release", "--bins"])?;
            super::target_dir().join("release/rusty")
        }
    };
    super::sh("cargo", &["build", "--release", "--example", "perf"])?;
    let perf = super::target_dir().join("release/examples/perf");
    let mut bench = Bench { binary, runs: runs.max(1), only, rows: Vec::new(), report: serde_json::Map::new() };
    bench.run_all(&perf)?;
    bench.print();
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let report = json!({"binary": bench.binary, "runs": bench.runs, "results": bench.report});
    std::fs::write(&out, serde_json::to_string_pretty(&report)? + "\n")?;
    println!("\nwrote {}", out.display());
    Ok(ExitCode::SUCCESS)
}

struct Bench {
    binary: PathBuf,
    runs: usize,
    only: Vec<String>,
    rows: Vec<[String; 5]>,
    report: serde_json::Map<String, Value>,
}

/// One finished process.
struct Sample {
    wall_ms: f64,
    cpu_ms: f64,
    rss: u64,
    ok: bool,
}

impl Bench {
    fn wants(&self, group: &str) -> bool {
        self.only.is_empty() || self.only.iter().any(|g| g == group)
    }

    fn run_all(&mut self, perf: &Path) -> Result<()> {
        let tmp = Scratch::new()?;
        let home = tmp.0.join("h");
        let work = tmp.0.join("w");
        std::fs::create_dir_all(&work)?;
        let work = work.canonicalize()?;

        for (name, args) in self
            .wants("startup")
            .then_some([
                ("startup --help", &["--help"][..]),
                ("startup --version", &["--version"]),
                ("startup --capabilities", &["--capabilities"]),
                ("startup --sessions (none)", &["--sessions"]),
            ])
            .into_iter()
            .flatten()
        {
            let m = self.repeat(|| self.rusty(&home, &work, args, None))?;
            self.row(name, &m, String::new(), json!({}));
        }

        if self.wants("sessions") {
            for n in [500, 5000] {
                self.sessions(&home, &work, n)?;
            }
        }
        if self.wants("redact") {
            self.kernel(perf, "redact", |v| {
                format!(
                    "{:.0} MB/s plain, {:.0} MB/s handles, {} allocs",
                    v["mb_per_s"].as_f64().unwrap_or(0.0),
                    v["handles_mb_per_s"].as_f64().unwrap_or(0.0),
                    v["allocs"]
                )
            })?;
        }
        if self.wants("recall") {
            self.kernel(perf, "recall", |v| {
                format!(
                    "build {:.1} ms ({} allocs), terms {:.1} ms, query {:.0} us",
                    v["build_ms"].as_f64().unwrap_or(0.0),
                    v["build_allocs"],
                    v["terms_ms"].as_f64().unwrap_or(0.0),
                    v["query_us"].as_f64().unwrap_or(0.0)
                )
            })?;
        }
        if self.wants("turn") {
            self.context(&home, &work)?;
        }
        if self.wants("stream") {
            self.stream(&home, &work)?;
        }
        if !self.wants("size") {
            return Ok(());
        }

        let dir = self.binary.parent().unwrap_or(Path::new("."));
        let mut sizes = serde_json::Map::new();
        for bin in ["rusty", "rusty-memoryd", "rusty-cloud"] {
            if let Ok(meta) = std::fs::metadata(dir.join(bin)) {
                sizes.insert(bin.into(), meta.len().into());
                self.rows.push([
                    format!("binary {bin}"),
                    String::new(),
                    String::new(),
                    String::new(),
                    format!("{:.2} MB", meta.len() as f64 / 1e6),
                ]);
            }
        }
        self.report.insert("binary_bytes".into(), sizes.into());
        Ok(())
    }

    /// `--sessions` and a search over `n` synthetic transcripts.
    fn sessions(&mut self, home: &Path, work: &Path, n: usize) -> Result<()> {
        let dir = sessions_dir(home, work);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let mut rng = Rng(n as u64);
        let mut transcript_bytes = 0;
        for i in 0..n {
            let history: Vec<Value> = (0..20)
                .map(|m| {
                    let role = if m % 2 == 0 { "user" } else { "assistant" };
                    json!({"role": role, "content": sentence(&mut rng, 40, i)})
                })
                .collect();
            let body = json!({"started": 1_700_000_000 + i as u64, "model": "bench/model", "history": history});
            let text = body.to_string();
            transcript_bytes += text.len() as u64;
            std::fs::write(dir.join(format!("{:032x}.json", 0x5e55_0000_0000u64 + i as u64)), text)?;
        }
        // The first listing builds the catalog from every transcript.
        let cold = self.rusty(home, work, &["--sessions"], None)?;
        let index = std::fs::metadata(dir.join("index.sqlite")).map(|m| m.len()).unwrap_or(0);
        let extra =
            format!("index {:.1} MB for {:.1} MB of transcripts", index as f64 / 1e6, transcript_bytes as f64 / 1e6);
        let data = json!({"sessions": n, "index_bytes": index, "transcript_bytes": transcript_bytes});
        self.row(&format!("sessions cold index ({n})"), &cold, extra, data.clone());
        let m = self.repeat(|| self.rusty(home, work, &["--sessions"], None))?;
        self.row(&format!("sessions list ({n})"), &m, String::new(), data.clone());
        let m = self.repeat(|| self.rusty(home, work, &["--sessions", "billing", "fixture"], None))?;
        self.row(&format!("sessions search ({n})"), &m, String::new(), data);
        std::fs::remove_dir_all(&dir)?;
        Ok(())
    }

    fn kernel(&mut self, perf: &Path, name: &str, summary: impl Fn(&Value) -> String) -> Result<()> {
        let mut cmd = Command::new(perf);
        cmd.args([name, "7"]).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let (sample, out) = measure(cmd)?;
        if !sample.ok {
            bail!("the {name} kernel failed");
        }
        let v: Value = serde_json::from_str(out.trim()).with_context(|| format!("{name} printed {out}"))?;
        let cpu = v["ms"].as_f64().or(v["build_ms"].as_f64()).unwrap_or(0.0);
        let label = if name == "redact" { "redact 10 MB" } else { "recall 5000 lessons" };
        // The kernel reports its own thread CPU time, not the process's.
        let m = Sample { wall_ms: f64::NAN, cpu_ms: cpu, ..sample };
        self.row(label, &m, summary(&v), v);
        Ok(())
    }

    /// A resumed 300-message session doing ten read_file turns: the time
    /// between a reply ending and the next request arriving is rusty's own
    /// per-turn work (parse, tool, context assembly, save, send).
    fn context(&mut self, home: &Path, work: &Path) -> Result<()> {
        for f in 0..10 {
            std::fs::write(work.join(format!("f{f}.txt")), format!("file {f}\n"))?;
        }
        let dir = sessions_dir(home, work);
        let mut rng = Rng(300);
        let history: Vec<Value> = (0..300)
            .map(|m| {
                let role = if m % 2 == 0 { "user" } else { "assistant" };
                json!({"role": role, "content": sentence(&mut rng, 90, m)})
            })
            .collect();
        let id = "c0ffee00c0ffee00c0ffee00c0ffee00";
        let session = json!({"started": 1_700_000_000u64, "model": "bench/model", "history": history});
        let mut gaps = Vec::new();
        let mut last_body = 0;
        let mut samples = Vec::new();
        let mut bodies: Vec<String> = (0..10)
            .map(|i| {
                let args = json!({"path": format!("f{i}.txt")}).to_string();
                let reply = json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("call{i}"),
                    "type": "function", "function": {"name": "read_file", "arguments": args}}]},
                    "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 40000, "completion_tokens": 5}});
                format!("data: {reply}\n\ndata: [DONE]\n\n")
            })
            .collect();
        bodies.push(say("done"));
        let bodies = Arc::new(bodies);
        for _ in 0..=self.runs.min(5) {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join(format!("{id}.json")), session.to_string())?;
            let mock = Mock::start(bodies.clone())?;
            let s = self.rusty(home, work, &["-c", id, "read the ten files"], Some(&mock.url))?;
            let log = mock.stop();
            if !s.ok || log.len() < 11 {
                bail!("the context run made {} requests (exit ok: {})", log.len(), s.ok);
            }
            gaps.extend(log.windows(2).map(|w| (w[1].received - w[0].answered).as_secs_f64() * 1e3));
            last_body = log.last().map_or(0, |r| r.body_bytes);
            samples.push(s);
            std::fs::remove_dir_all(&dir)?;
        }
        samples.remove(0);
        let m = medians(&samples);
        let turn = median(gaps);
        let extra = format!("{turn:.2} ms per turn, request {:.0} KB", last_body as f64 / 1e3);
        let data = json!({"messages": 300, "turns": 10, "turn_ms": turn, "request_bytes": last_body});
        self.row("turn, 300-message history", &m, extra, data);
        Ok(())
    }

    /// A 5 MB answer in 50,000 deltas.
    fn stream(&mut self, home: &Path, work: &Path) -> Result<()> {
        let mut samples = Vec::new();
        let mut body = String::with_capacity(7 << 20);
        for i in 0..50_000 {
            let mut text = format!("word{i:06} ");
            while text.len() < 100 {
                text.push_str("lorem ipsum ");
            }
            text.truncate(99);
            text.push(if i % 8 == 7 { '\n' } else { ' ' });
            body.push_str(&format!("data: {}\n\n", json!({"choices": [{"delta": {"content": text}}]})));
        }
        body.push_str(&say(""));
        let bodies = Arc::new(vec![body]);
        for _ in 0..=self.runs.min(5) {
            let mock = Mock::start(bodies.clone())?;
            let s = self.rusty(home, work, &["write it all"], Some(&mock.url))?;
            mock.stop();
            if !s.ok {
                bail!("the stream run failed");
            }
            samples.push(s);
        }
        samples.remove(0);
        let m = medians(&samples);
        self.row("stream 5 MB answer", &m, format!("{:.1} MB/s", 5.0 / (m.wall_ms / 1e3)), json!({"bytes": 5_000_000}));
        Ok(())
    }

    /// One rusty process with an offline environment.
    fn rusty(&self, home: &Path, work: &Path, args: &[&str], url: Option<&str>) -> Result<Sample> {
        let mut cmd = Command::new(&self.binary);
        cmd.env_clear()
            .current_dir(work)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", home)
            .env("RUSTY_HOME", home)
            .env("RUSTY_NO_DOTENV", "1")
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("NVIDIA_API_KEY", "offline-bench-key")
            .env("RUSTY_CONTEXT_TOKENS", "128000")
            .args(["--memory", "off", "--mode", "standard"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // --memory and --mode are harmless before --help or --version.
        if let Some(url) = url {
            cmd.env("RUSTY_BASE_URL", url);
        }
        Ok(measure(cmd)?.0)
    }

    /// One warm-up, then the medians of `runs` samples.
    fn repeat(&self, mut f: impl FnMut() -> Result<Sample>) -> Result<Sample> {
        f()?;
        let samples = (0..self.runs).map(|_| f()).collect::<Result<Vec<_>>>()?;
        if samples.iter().any(|s| !s.ok) {
            bail!("a benchmarked command failed");
        }
        Ok(medians(&samples))
    }

    fn row(&mut self, name: &str, m: &Sample, extra: String, mut data: Value) {
        data["wall_ms"] = json!((m.wall_ms * 1000.0).round() / 1000.0);
        data["cpu_ms"] = json!((m.cpu_ms * 1000.0).round() / 1000.0);
        data["rss_bytes"] = m.rss.into();
        self.report.insert(name.into(), data);
        self.rows.push([
            name.into(),
            if m.wall_ms.is_nan() { "-".into() } else { format!("{:.2}", m.wall_ms) },
            format!("{:.2}", m.cpu_ms),
            format!("{:.1}", m.rss as f64 / (1 << 20) as f64),
            extra,
        ]);
    }

    fn print(&self) {
        let head = ["metric", "wall ms", "cpu ms", "RSS MB", "notes"];
        let width = |c: usize| self.rows.iter().map(|r| r[c].len()).chain([head[c].len()]).max().unwrap_or(0);
        let (a, b, c, d) = (width(0), width(1), width(2), width(3));
        println!("\n| {:a$} | {:>b$} | {:>c$} | {:>d$} | {}", head[0], head[1], head[2], head[3], head[4]);
        println!("|{}|{}|{}|{}|---", "-".repeat(a + 2), "-".repeat(b + 2), "-".repeat(c + 2), "-".repeat(d + 2));
        for r in &self.rows {
            println!("| {:a$} | {:>b$} | {:>c$} | {:>d$} | {}", r[0], r[1], r[2], r[3], r[4]);
        }
    }
}

/// Runs `cmd` to completion; its wall time, peak RSS and stdout.
fn measure(mut cmd: Command) -> Result<(Sample, String)> {
    let start = Instant::now();
    let mut child = cmd.spawn().context("starting a benchmarked command")?;
    let reader = child.stdout.take().map(|mut out| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s);
            s
        })
    });
    let mut status = 0;
    // SAFETY: rusage is plain data, and the pid is our own unreaped child.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let pid = unsafe { libc::wait4(child.id() as libc::pid_t, &mut status, 0, &mut usage) };
    let wall_ms = start.elapsed().as_secs_f64() * 1e3;
    if pid < 0 {
        bail!("wait4: {}", std::io::Error::last_os_error());
    }
    // macOS reports bytes, Linux kilobytes.
    let rss = if cfg!(target_os = "macos") { usage.ru_maxrss as u64 } else { usage.ru_maxrss as u64 * 1024 };
    let ok = libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0;
    let ms = |t: libc::timeval| t.tv_sec as f64 * 1e3 + t.tv_usec as f64 / 1e3;
    let cpu_ms = ms(usage.ru_utime) + ms(usage.ru_stime);
    let out = reader.map(|r| r.join().unwrap_or_default()).unwrap_or_default();
    Ok((Sample { wall_ms, cpu_ms, rss, ok }, out))
}

/// The median of each measure; `ok` only if every run succeeded.
fn medians(samples: &[Sample]) -> Sample {
    Sample {
        wall_ms: median(samples.iter().map(|s| s.wall_ms).collect()),
        cpu_ms: median(samples.iter().map(|s| s.cpu_ms).collect()),
        rss: median(samples.iter().map(|s| s.rss as f64).collect()) as u64,
        ok: samples.iter().all(|s| s.ok),
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// Where rusty keeps this project's transcripts (main.rs `project_dir`).
fn sessions_dir(home: &Path, work: &Path) -> PathBuf {
    let name = work.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "root".into());
    let hash = work
        .display()
        .to_string()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    home.join("projects").join(format!("{name}-{:08x}", hash as u32)).join("sessions")
}

/// A final text reply as one SSE body.
fn say(text: &str) -> String {
    let reply = json!({"choices": [{"delta": {"content": text}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}});
    format!("data: {reply}\n\ndata: [DONE]\n\n")
}

/// A fixed LCG, so every run sees the same text.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

const WORDS: &[&str] = &[
    "parser",
    "config",
    "request",
    "session",
    "history",
    "token",
    "cache",
    "index",
    "render",
    "stream",
    "billing",
    "fixture",
    "manifest",
    "handler",
    "retry",
    "budget",
    "context",
    "window",
    "lesson",
    "the",
    "and",
    "then",
    "fix",
    "rounding",
    "deploy",
    "migration",
    "tests",
    "module",
    "service",
    "query",
];

fn sentence(rng: &mut Rng, words: usize, salt: usize) -> String {
    let mut s = format!("step {salt}:");
    for _ in 0..words {
        s.push(' ');
        s.push_str(WORDS[rng.next() as usize % WORDS.len()]);
    }
    s
}

struct Request {
    received: Instant,
    answered: Instant,
    body_bytes: usize,
}

/// A local OpenAI-style endpoint. The i-th POST gets `bodies[i]` (the last
/// one once they run out), built beforehand so only rusty's work is timed.
struct Mock {
    url: String,
    addr: std::net::SocketAddr,
    log: Arc<Mutex<Vec<Request>>>,
    done: Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Mock {
    fn start(bodies: Arc<Vec<String>>) -> Result<Mock> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let log = Arc::new(Mutex::new(Vec::new()));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (log2, done2) = (log.clone(), done.clone());
        let thread = std::thread::spawn(move || {
            let mut posts = 0;
            for stream in listener.incoming() {
                if done2.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                if let Ok(Some(r)) = serve(stream, &bodies, &mut posts) {
                    log2.lock().unwrap().push(r);
                }
            }
        });
        Ok(Mock { url: format!("http://{addr}/v1"), addr, log, done, thread })
    }

    fn stop(self) -> Vec<Request> {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        let _ = self.thread.join();
        std::mem::take(&mut *self.log.lock().unwrap())
    }
}

fn serve(stream: TcpStream, bodies: &[String], posts: &mut usize) -> Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let (mut size, mut first) = (0, String::new());
    reader.read_line(&mut first)?;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                size = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0; size];
    reader.read_exact(&mut body)?;
    let received = Instant::now();
    let mut out = std::io::BufWriter::with_capacity(64 << 10, stream);
    if !first.starts_with("POST") {
        let data = r#"{"data":[]}"#;
        write!(out, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len())?;
        return Ok(None);
    }
    write!(out, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")?;
    if let Some(body) = bodies.get(*posts).or(bodies.last()) {
        out.write_all(body.as_bytes())?;
    }
    out.flush()?;
    *posts += 1;
    Ok(Some(Request { received, answered: Instant::now(), body_bytes: size }))
}

/// A short directory under the system temp dir, removed on drop.
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Result<Scratch> {
        let path = std::env::temp_dir().join(format!("rbench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path)?;
        Ok(Scratch(path.canonicalize()?))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
