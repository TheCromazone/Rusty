//! How well `locate` finds the files a change touches, against grep, on a
//! repository's own history. Each sampled commit's subject is the query
//! (its `type(scope):`, `[tag]` and `(#123)` parts removed unless `--scope`),
//! the corpus is the tree of its parent, read from git objects so the fix
//! itself never leaks in, and the answer is the set of files it modified.
//!
//! It reports precision as well as recall, since every wrong file shown
//! costs the agent context; a perfect ranking's precision is printed too,
//! because a commit changing two files can fill only two of five places.
//!
//! Two greps stand in for what an agent does without `locate`:
//! - grep-rarest: one search for the query word in the fewest files, files
//!   in path order, which is how a first grep usually goes;
//! - grep-all: every query word searched, files ranked by how many distinct
//!   words they contain, a stronger baseline than most agents run.
//!
//! Each snapshot gets a fresh index, as each `locate` call does. Its build
//! time counts tokenizing and indexing, not reading the files.
//!
//!     cargo run --release --example locate_bench -- REPO [QUERIES] [--scope]
//!
//! Set GIT_NO_LAZY_FETCH=1 for a partial clone, so git never downloads
//! missing objects; their files are then left out.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::time::Instant;

use rusty::locate::{render, worth_indexing, Index};
use rusty::recall;

const SHOWN: usize = 8;

fn git(repo: &str, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(repo).args(args).output().expect("git runs");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Blob contents through one `git cat-file --batch`.
struct Blobs {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
}

impl Blobs {
    fn new(repo: &str) -> Self {
        let mut child = Command::new("git")
            .args(["-C", repo, "cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("git cat-file runs");
        let out = BufReader::new(child.stdout.take().unwrap());
        Blobs { child, out }
    }

    fn text(&mut self, id: &str) -> Option<String> {
        writeln!(self.child.stdin.as_mut().unwrap(), "{id}").ok()?;
        let mut header = String::new();
        self.out.read_line(&mut header).ok()?;
        if header.trim_end().ends_with(" missing") {
            return None;
        }
        let size: usize = header.split_whitespace().nth(2)?.parse().ok()?;
        let mut buf = vec![0; size + 1];
        self.out.read_exact(&mut buf).ok()?;
        buf.truncate(size);
        (!buf[..size.min(8192)].contains(&0)).then(|| String::from_utf8_lossy(&buf).into_owned())
    }
}

/// The commit subject as a person might ask it.
fn question(subject: &str, keep_scope: bool) -> String {
    let mut s = subject.trim();
    if let Some(i) = s.rfind(" (#").filter(|_| s.ends_with(')')) {
        s = &s[..i];
    }
    if !keep_scope {
        while let Some(rest) = s.strip_prefix('[').and_then(|r| r.split_once(']')).map(|(_, r)| r.trim_start()) {
            s = rest;
        }
        if let Some((_, rest)) = s.split_once(": ").filter(|(head, _)| !head.contains(' ') && head.len() < 30) {
            s = rest;
        }
    }
    s.to_string()
}

fn indexable(path: &str) -> bool {
    let binary = [".png", ".jpg", ".gif", ".ico", ".woff", ".woff2", ".ttf", ".pdf", ".zip", ".gz", ".wasm", ".svg"];
    !binary.iter().any(|e| path.ends_with(e)) && !path.to_lowercase().contains("changelog")
}

struct Case {
    parent: String,
    query: String,
    gold: Vec<String>,
}

#[derive(Default)]
struct Score {
    n: f64,
    hit1: f64,
    hit5: f64,
    recall5: f64,
    recall_shown: f64,
    precision5: f64,
    precision_shown: f64,
    /// What a perfect ranking would score at five and at `SHOWN`: a commit
    /// changing two files can fill at most two of the five places.
    best5: f64,
    best_shown: f64,
    mrr: f64,
}

impl Score {
    fn add(&mut self, ranked: &[&str], gold: &HashSet<&str>) {
        self.n += 1.0;
        let found: Vec<usize> = ranked.iter().enumerate().filter(|(_, p)| gold.contains(*p)).map(|(r, _)| r).collect();
        let within = |k: usize| found.iter().filter(|&&r| r < k).count() as f64;
        let share = |k: usize| within(k) / gold.len() as f64;
        self.hit1 += f64::from(found.first() == Some(&0));
        self.hit5 += f64::from(found.first().is_some_and(|&r| r < 5));
        self.recall5 += share(5);
        self.recall_shown += share(SHOWN);
        self.precision5 += within(5) / 5.0;
        self.precision_shown += within(SHOWN) / SHOWN as f64;
        self.best5 += gold.len().min(5) as f64 / 5.0;
        self.best_shown += gold.len().min(SHOWN) as f64 / SHOWN as f64;
        self.mrr += found.first().map_or(0.0, |&r| 1.0 / (r + 1) as f64);
    }

    fn row(&self, name: &str) -> String {
        let pct = |x: f64| 100.0 * x / self.n.max(1.0);
        format!(
            "{name:<12} hit@1 {:>5.1}  hit@5 {:>5.1}  recall@5 {:>5.1}  recall@{SHOWN} {:>5.1}  \
             precision@5 {:>5.1}  precision@{SHOWN} {:>5.1}  MRR {:.3}",
            pct(self.hit1),
            pct(self.hit5),
            pct(self.recall5),
            pct(self.recall_shown),
            pct(self.precision5),
            pct(self.precision_shown),
            self.mrr / self.n.max(1.0)
        )
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let keep_scope = args.iter().any(|a| a == "--scope");
    let mut positional = args.iter().filter(|a| !a.starts_with("--"));
    let Some(repo) = positional.next() else {
        eprintln!("usage: cargo run --release --example locate_bench -- REPO [QUERIES] [--scope]");
        std::process::exit(2);
    };
    let want: usize = positional.next().and_then(|n| n.parse().ok()).unwrap_or(100);

    // Single-parent commits with a subject of at least three terms, spread
    // evenly over the last 4,000, each modifying one to eight text files.
    let log = git(repo, &["log", "--no-merges", "--format=%H%x09%P%x09%s", "-n", "4000"]);
    let candidates: Vec<(&str, &str, String)> = log
        .lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            let (commit, parent, subject) = (f.next()?, f.next()?, f.next()?);
            let lower = subject.to_lowercase();
            let chore = ["bump ", "release", "prepare v", "merge"].iter().any(|p| lower.starts_with(p));
            let q = question(subject, keep_scope);
            (!parent.is_empty() && !parent.contains(' ') && !chore && recall::terms(&q).len() >= 3)
                .then_some((commit, parent, q))
        })
        .collect();
    let mut cases = Vec::new();
    for (commit, parent, query) in candidates.iter().step_by((candidates.len() / want).max(1)) {
        let status = git(repo, &["diff-tree", "--no-commit-id", "-r", "--name-status", "--no-renames", parent, commit]);
        let gold: Vec<String> =
            status.lines().filter_map(|l| l.strip_prefix("M\t")).filter(|p| indexable(p)).map(String::from).collect();
        if (1..=8).contains(&gold.len()) {
            cases.push(Case { parent: parent.to_string(), query: query.clone(), gold });
        }
        if cases.len() == want {
            break;
        }
    }

    let mut blobs = Blobs::new(repo);
    let mut texts: HashMap<String, Option<Rc<(String, String)>>> = HashMap::new();
    let (mut grep_rarest, mut grep_all, mut locate) = (Score::default(), Score::default(), Score::default());
    let (mut builds, mut searches, mut shown_chars, mut files) = (0.0, 0.0, 0usize, 0usize);
    for case in &cases {
        // The parent's tree: path -> blob, then an index of it.
        let mut tree: HashMap<String, String> = HashMap::new();
        for line in git(repo, &["ls-tree", "-r", "-l", &case.parent]).lines() {
            let Some((meta, path)) = line.split_once('\t') else { continue };
            let m: Vec<&str> = meta.split_whitespace().collect();
            if m.len() == 4 && m[1] == "blob" && m[3].parse::<u64>().unwrap_or(u64::MAX) <= 1 << 20 && indexable(path) {
                tree.insert(path.to_string(), m[2].to_string());
            }
        }
        for (path, id) in &tree {
            texts.entry(id.clone()).or_insert_with(|| {
                blobs.text(id).filter(|t| worth_indexing(path, t)).map(|t| Rc::new((t.to_lowercase(), t)))
            });
        }
        let clock = Instant::now();
        let mut index = Index::default();
        for (path, id) in &tree {
            if let Some(t) = &texts[id] {
                index.insert(path, t.1.clone());
            }
        }
        builds += clock.elapsed().as_secs_f64();
        files += index.len();

        let gold: HashSet<&str> = case.gold.iter().map(String::as_str).collect();
        let text = |p: &str| tree.get(p).and_then(|id| texts[id].clone());
        let mut paths: Vec<&str> = tree.keys().map(String::as_str).filter(|p| text(p).is_some()).collect();
        paths.sort_unstable();

        let mut words: Vec<String> = recall::terms(&case.query).into_iter().filter(|w| w.len() >= 3).collect();
        words.sort_unstable();
        words.dedup();
        let has = |p: &str, w: &str| text(p).is_some_and(|t| t.0.contains(w));
        let rarest =
            words.iter().map(|w| (paths.iter().filter(|p| has(p, w)).count(), w)).filter(|(n, _)| *n > 0).min();
        let ranked: Vec<&str> = match rarest {
            Some((_, w)) => paths.iter().copied().filter(|p| has(p, w)).collect(),
            None => Vec::new(),
        };
        grep_rarest.add(&ranked, &gold);
        let mut by_words: Vec<(usize, &str)> =
            paths.iter().map(|&p| (words.iter().filter(|w| has(p, w)).count(), p)).filter(|(n, _)| *n > 0).collect();
        by_words.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        grep_all.add(&by_words.iter().map(|(_, p)| *p).collect::<Vec<_>>(), &gold);

        let clock = Instant::now();
        let hits = index.search(&case.query, 10);
        searches += clock.elapsed().as_secs_f64();
        locate.add(&hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), &gold);
        let shown = &hits[..hits.len().min(SHOWN)];
        shown_chars += render(&index, shown, &case.query).len();
    }

    let n = cases.len().max(1) as f64;
    println!(
        "{repo}: {} queries, {} files on average, scope {}",
        cases.len(),
        files / cases.len().max(1),
        if keep_scope { "kept" } else { "removed" }
    );
    for (score, name) in [(&grep_rarest, "grep-rarest"), (&grep_all, "grep-all"), (&locate, "locate")] {
        println!("  {}", score.row(name));
    }
    println!(
        "  a perfect ranking: precision@5 {:.1}  precision@{SHOWN} {:.1}",
        100.0 * locate.best5 / n,
        100.0 * locate.best_shown / n
    );
    println!(
        "  locate output for {SHOWN} files: {:.0} characters on average (~{:.0} tokens)",
        shown_chars as f64 / n,
        shown_chars as f64 / n / 4.0
    );
    println!("  index build {:.0} ms · search {:.1} ms", builds * 1000.0 / n, searches * 1000.0 / n);
}
