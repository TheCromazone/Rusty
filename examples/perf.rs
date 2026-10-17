//! In-process kernels for `cargo xtask bench`: redaction and recall, timed
//! with the medians of several runs and counted with a global allocator.
//! Times are this thread's CPU time, which other load on the machine barely
//! moves; allocation counts are exact.
//! Prints one JSON object per kernel. Inputs come from a fixed seed, so two
//! builds see the same bytes.
//!
//!     cargo run --release --example perf -- redact|recall [RUNS]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

struct Counting;
static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size() as u64, Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(new as u64, Relaxed);
        System.realloc(ptr, layout, new)
    }
}

#[global_allocator]
static A: Counting = Counting;

/// A tiny LCG: the same sequence on every machine and Rust version.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.next() as usize % items.len()]
    }
}

const WORDS: &[&str] = &[
    "parser", "config", "request", "session", "history", "token", "cache", "index", "render", "stream", "billing",
    "tax_rate", "fixture", "manifest", "handler", "retry", "budget", "context", "window", "lesson",
];

/// About `size` bytes of tool output: code, logs, JSON and, every few
/// lines, something the redactor must catch.
fn mixed_text(size: usize) -> String {
    let mut rng = Rng(42);
    let mut out = String::with_capacity(size + 256);
    while out.len() < size {
        let (a, b) = (rng.pick(WORDS), rng.pick(WORDS));
        let n = rng.next() % 1000;
        let line = match rng.next() % 12 {
            0 => format!("export API_KEY=sk-{n:04}abcdefghijklmnopqrstuv{a}\n"),
            1 => format!("Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.{a}{n}.signature\n"),
            2 => format!("git clone https://user:hunter{n}@example.com/{a}/{b}.git\n"),
            3 => format!("{{\"{a}\": {n}, \"password\": \"pw-{b}-{n}\", \"ok\": true}}\n"),
            4 => format!("    let {a}_{b} = self.{a}.get({n}).map(|x| x.{b}()).unwrap_or_default();\n"),
            5 => format!("2026-10-17T12:00:{:02}Z INFO {a}: handled {b} in {n}ms\n", n % 60),
            6 => format!("src/{a}/{b}.rs:{n}:5: warning: unused variable `{a}`\n"),
            _ => format!("The {a} keeps the {b} for request {n}, then flushes the {a} cache.\n"),
        };
        out.push_str(&line);
    }
    out
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// This thread's CPU time in ms.
fn cpu_ms() -> f64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid out pointer for a clock every supported Unix has.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut t) };
    t.tv_sec as f64 * 1e3 + t.tv_nsec as f64 / 1e6
}

/// Runs `f` `runs` times; the median CPU ms and the allocations of one run.
fn measure(runs: usize, mut f: impl FnMut()) -> (f64, u64, u64) {
    f(); // warm up caches and lazy statics
    let mut times = Vec::with_capacity(runs);
    let (mut allocs, mut bytes) = (0, 0);
    for i in 0..runs {
        let (a0, b0) = (ALLOCS.load(Relaxed), BYTES.load(Relaxed));
        let start = cpu_ms();
        f();
        times.push(cpu_ms() - start);
        if i == 0 {
            (allocs, bytes) = (ALLOCS.load(Relaxed) - a0, BYTES.load(Relaxed) - b0);
        }
    }
    (median(times), allocs, bytes)
}

fn redact(runs: usize) {
    let text = mixed_text(10 << 20);
    let mb = text.len() as f64 / (1 << 20) as f64;
    let mut found = 0;
    let (ms, allocs, bytes) = measure(runs, || found = rusty::privacy::redact(&text).1);
    let (hms, hallocs, hbytes) = measure(runs, || {
        std::hint::black_box(rusty::privacy::redact_with_handles(&text));
    });
    println!(
        "{}",
        serde_json::json!({
            "kernel": "redact", "input_bytes": text.len(), "secrets": found,
            "ms": ms, "mb_per_s": mb / (ms / 1e3), "allocs": allocs, "alloc_bytes": bytes,
            "handles_ms": hms, "handles_mb_per_s": mb / (hms / 1e3),
            "handles_allocs": hallocs, "handles_alloc_bytes": hbytes,
        })
    );
}

fn recall(runs: usize) {
    use rusty::recall::{query, terms, Index};
    let mut rng = Rng(7);
    let lessons: Vec<String> = (0..5000)
        .map(|i| {
            let (a, b, c) = (rng.pick(WORDS), rng.pick(WORDS), rng.pick(WORDS));
            format!(
                "Lesson {i}: before touching {a} in src/{b}/mod{i}.rs run the {c} checks; \
                 parseConfig{i} rounds the {b} and the {a}_{c} fixture must be regenerated."
            )
        })
        .collect();
    let mut docs = Vec::new();
    let (terms_ms, terms_allocs, _) = measure(runs, || docs = lessons.iter().map(|l| terms(l)).collect());
    let mut index = Index::new(&docs);
    let (build_ms, build_allocs, build_bytes) = measure(runs, || index = Index::new(&docs));
    let questions: Vec<_> = (0..200)
        .map(|i| query(&format!("fix the {} {} in mod{i} and rerun", rng.pick(WORDS), rng.pick(WORDS))))
        .collect();
    let mut i = 0;
    let (query_ms, query_allocs, _) = measure(runs.max(50), || {
        std::hint::black_box(index.scores(&questions[i % questions.len()]));
        i += 1;
    });
    println!(
        "{}",
        serde_json::json!({
            "kernel": "recall", "lessons": lessons.len(),
            "terms_ms": terms_ms, "terms_allocs": terms_allocs,
            "build_ms": build_ms, "build_allocs": build_allocs, "build_alloc_bytes": build_bytes,
            "query_us": query_ms * 1e3, "query_allocs": query_allocs,
        })
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runs = args.get(1).and_then(|n| n.parse().ok()).unwrap_or(7);
    match args.first().map(String::as_str) {
        Some("redact") => redact(runs),
        Some("recall") => recall(runs),
        _ => {
            eprintln!("usage: perf redact|recall [RUNS]");
            std::process::exit(2);
        }
    }
}
