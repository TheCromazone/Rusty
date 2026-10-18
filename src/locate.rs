//! Ranked code search for the `locate` tool: it finds code from a
//! description when the names to grep for aren't known.
//!
//! Grep answers "where does this exact text appear", in path order. A
//! question like "where do we retry a dropped stream" needs the places where
//! most of its words meet, rarest words first. That is BM25, scored twice:
//!
//! - over chunks, each running from one definition to the next (at most
//!   [`CHUNK_LINES`] lines), with the file's path words counted in every
//!   chunk, so "codex refresh" finds the refresh function in `src/codex.rs`;
//! - over whole files, which wins when the words are spread across a module.
//!
//! The two rankings are fused by reciprocal rank. Identifiers are split as
//! in [`crate::recall`], so "tax rate" meets `taxRate`.
//!
//! `examples/locate_bench.rs` measures this against grep on a repository's
//! history, asking each commit's message for the files it changed. On
//! repositories of 1,500 to 2,000 files it put a changed file first two to
//! three times as often as a grep for every query word, ranked by how many
//! each file matched. On 200-file ones it still did at rank one, but that
//! grep found a changed file in its top five about as often.
//! docs/SEARCH.md has the numbers and what they did for an agent.
//!
//! Like the other workspace queries (docs/WORKSPACE_QUERIES.md), it keeps
//! nothing between calls: [`index`] reads every file a search would read
//! (`search_files`), the search ranks them, and the index is dropped. Lines
//! are shown from the same reading that was ranked.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::recall;
use crate::workspace_query::Enumeration;

/// Longest chunk; a longer definition is split.
const CHUNK_LINES: usize = 60;
/// Bigger files are left out: generated code, data and bundles.
const MAX_FILE: u64 = 1 << 20;
/// Lines longer than this on average mark minified or generated text.
const MAX_AVG_LINE: usize = 300;
/// Reciprocal rank fusion's constant, from Cormack, Clarke and Büttcher (2009).
const RRF_K: f64 = 60.0;
const K1: f64 = 1.2;
const B: f64 = 0.75;
/// Chunks shown per file, and lines shown per chunk besides its first.
const CHUNKS_SHOWN: usize = 2;
const LINES_SHOWN: usize = 3;

/// Lines that start a definition, after up to eight spaces of indentation.
/// `outline` lists these, and chunks begin at them.
const DEF_PREFIXES: &[&str] = &[
    "pub fn ",
    "fn ",
    "pub(crate) fn ",
    "async fn ",
    "pub async fn ",
    "pub struct ",
    "struct ",
    "pub enum ",
    "enum ",
    "pub trait ",
    "trait ",
    "impl ",
    "impl<",
    "pub mod ",
    "mod ",
    "macro_rules!",
    "pub type ",
    "pub const ",
    "def ",
    "async def ",
    "class ",
    "function ",
    "async function ",
    "export function ",
    "export async function ",
    "export default ",
    "export class ",
    "export const ",
    "export interface ",
    "export type ",
    "interface ",
    "func ",
    "public ",
    "private ",
    "protected ",
    "module ",
];

/// Whether `line` starts a definition: a function, type, class or module.
pub fn is_definition(line: &str) -> bool {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 8 || !DEF_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
        return false;
    }
    // `public`/`private` lines without a parameter list are fields.
    let access = ["public ", "private ", "protected "].iter().any(|p| trimmed.starts_with(p));
    !access || trimmed.contains('(')
}

/// Whether the text at `path` belongs in the index. Never secrets or lock
/// files, whose package names would match nearly any query, whatever the
/// ignore rules say; nor oversized, binary or minified text, nor terminal
/// recordings full of escape codes.
pub fn worth_indexing(path: &str, text: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    let excluded = name.starts_with(".env")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".lock")
        || ["package-lock.json", "pnpm-lock.yaml", "yarn.lock", "go.sum"].contains(&name);
    !excluded
        && text.len() as u64 <= MAX_FILE
        && !text.as_bytes()[..text.len().min(8192)].iter().any(|&b| b == 0 || b == 0x1b)
        && text.len() / text.lines().count().max(1) <= MAX_AVG_LINE
}

/// Term counts, sorted by term id.
type Counts = Vec<(u32, u32)>;

fn count(of: &Counts, term: u32) -> u32 {
    of.binary_search_by_key(&term, |&(t, _)| t).map_or(0, |i| of[i].1)
}

struct Chunk {
    /// First line, from 0, and the line after the last.
    start: usize,
    end: usize,
    terms: u32,
    counts: Counts,
}

struct File {
    text: String,
    terms: u32,
    counts: Counts,
    /// The path's own terms, which count in every chunk.
    path_terms: u32,
    path_counts: Counts,
    chunks: Vec<Chunk>,
}

/// A file worth reading, with the line ranges that matched best.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub path: String,
    /// Line ranges, from 0 and end-exclusive, best first.
    pub spans: Vec<(usize, usize)>,
}

#[derive(Default)]
pub struct Index {
    vocab: HashMap<String, u32>,
    files: HashMap<String, File>,
}

impl Index {
    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    fn counts(&mut self, terms: Vec<String>) -> (u32, Counts) {
        let n = terms.len() as u32;
        let mut ids: Vec<u32> = terms
            .into_iter()
            .map(|t| {
                let next = self.vocab.len() as u32;
                *self.vocab.entry(t).or_insert(next)
            })
            .collect();
        ids.sort_unstable();
        let counts = ids.chunk_by(|a, b| a == b).map(|run| (run[0], run.len() as u32)).collect();
        (n, counts)
    }

    /// Indexes `text` as the file at `path`, replacing what was there.
    pub fn insert(&mut self, path: &str, text: String) {
        // Byte offset of each line's start, and one past the end.
        let mut offsets: Vec<usize> = std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        if offsets.last() != Some(&text.len()) {
            offsets.push(text.len());
        }
        let lines = offsets.len() - 1;
        let line = |i: usize| &text[offsets[i]..offsets[i + 1]];
        let mut starts: Vec<usize> = (1..lines).filter(|&i| is_definition(line(i))).collect();
        starts.insert(0, 0);
        starts.push(lines);
        let mut chunks = Vec::new();
        for w in starts.windows(2) {
            let mut start = w[0];
            while start < w[1] {
                let end = (start + CHUNK_LINES).min(w[1]);
                let (n, counts) = self.counts(recall::terms(&text[offsets[start]..offsets[end]]));
                if n > 0 {
                    chunks.push(Chunk { start, end, terms: n, counts });
                }
                start = end;
            }
        }
        // The file's counts are its chunks' counts added up.
        let mut all: Counts = chunks.iter().flat_map(|c| c.counts.iter().copied()).collect();
        all.sort_unstable_by_key(|&(t, _)| t);
        let counts: Counts =
            all.chunk_by(|a, b| a.0 == b.0).map(|run| (run[0].0, run.iter().map(|r| r.1).sum())).collect();
        let terms = chunks.iter().map(|c| c.terms).sum();
        let (path_terms, path_counts) = self.counts(recall::terms(path));
        self.files.insert(path.to_string(), File { text, terms, counts, path_terms, path_counts, chunks });
    }

    /// The `limit` files that best match `query`, best first.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let mut q: Vec<u32> = recall::query(query).iter().filter_map(|t| self.vocab.get(t).copied()).collect();
        q.sort_unstable();
        let mut files: Vec<(&String, &File)> = self.files.iter().collect();
        if q.is_empty() || files.is_empty() {
            return Vec::new();
        }
        files.sort_by(|a, b| a.0.cmp(b.0));

        // Corpus statistics, then each query term's document frequency at
        // both levels. A term in a file's path is in every one of its chunks.
        let n_files = files.len() as f64;
        let n_chunks = files.iter().map(|(_, f)| f.chunks.len()).sum::<usize>().max(1) as f64;
        let avg_file = files.iter().map(|(_, f)| f.terms as f64).sum::<f64>() / n_files;
        let avg_chunk =
            files.iter().flat_map(|(_, f)| f.chunks.iter().map(|c| (c.terms + f.path_terms) as f64)).sum::<f64>()
                / n_chunks;
        let (mut df_file, mut df_chunk) = (vec![0usize; q.len()], vec![0usize; q.len()]);
        for (_, f) in &files {
            for (i, &t) in q.iter().enumerate() {
                if count(&f.counts, t) > 0 {
                    df_file[i] += 1;
                }
                df_chunk[i] += if count(&f.path_counts, t) > 0 {
                    f.chunks.len()
                } else {
                    f.chunks.iter().filter(|c| count(&c.counts, t) > 0).count()
                };
            }
        }
        // Smoothed IDF, as in recall: a term every file shares still counts.
        let idf = |n: f64, df: usize| ((1.0 + n) / (1.0 + df as f64)).ln() + 1.0;
        let bm25 = |tf: u32, len: f64, avg: f64| {
            let tf = tf as f64;
            tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * len / avg.max(1.0)))
        };

        let mut file_scores = vec![0.0; files.len()];
        // Each file's chunks with a score, best first.
        let mut chunk_scores: Vec<Vec<(f64, usize)>> = vec![Vec::new(); files.len()];
        for (fi, (_, f)) in files.iter().enumerate() {
            for (i, &t) in q.iter().enumerate() {
                let tf = count(&f.counts, t);
                if tf > 0 {
                    file_scores[fi] += idf(n_files, df_file[i]) * bm25(tf, f.terms as f64, avg_file);
                }
            }
            for (ci, c) in f.chunks.iter().enumerate() {
                let len = (c.terms + f.path_terms) as f64;
                let score: f64 = q
                    .iter()
                    .enumerate()
                    .map(|(i, &t)| match count(&c.counts, t) + count(&f.path_counts, t) {
                        0 => 0.0,
                        tf => idf(n_chunks, df_chunk[i]) * bm25(tf, len, avg_chunk),
                    })
                    .sum();
                if score > 0.0 {
                    chunk_scores[fi].push((score, ci));
                }
            }
            chunk_scores[fi].sort_by(|a, b| b.0.total_cmp(&a.0));
        }

        // Each ranking orders files by its own score; fusion adds 1/(k + rank).
        let best_chunk = |fi: usize| chunk_scores[fi].first().map_or(0.0, |c| c.0);
        let rankings: [&dyn Fn(usize) -> f64; 2] = [&|fi| file_scores[fi], &best_chunk];
        let mut fused = vec![0.0; files.len()];
        for score in rankings {
            let mut order: Vec<usize> = (0..files.len()).filter(|&fi| score(fi) > 0.0).collect();
            order.sort_by(|&a, &b| score(b).total_cmp(&score(a)));
            for (rank, fi) in order.into_iter().enumerate() {
                fused[fi] += 1.0 / (RRF_K + rank as f64);
            }
        }
        let mut order: Vec<usize> = (0..files.len()).filter(|&fi| fused[fi] > 0.0).collect();
        order.sort_by(|&a, &b| fused[b].total_cmp(&fused[a]));
        order
            .into_iter()
            .take(limit)
            .map(|fi| {
                let chunks = &files[fi].1.chunks;
                let top = best_chunk(fi);
                let spans = chunk_scores[fi]
                    .iter()
                    .take_while(|(s, _)| *s >= top / 2.0)
                    .take(CHUNKS_SHOWN)
                    .map(|&(_, ci)| (chunks[ci].start, chunks[ci].end))
                    .collect();
                Hit { path: files[fi].0.clone(), spans }
            })
            .collect()
    }
}

/// Reads every file `scan` lists, now, and indexes those
/// [`worth_indexing`]. Paths are kept as listed, less a leading `./`. A file
/// that can't be read is recorded in `scan`, which then reports itself
/// incomplete; files left out by policy are not.
pub fn index(scan: &mut Enumeration) -> Index {
    let mut index = Index::default();
    for path in std::mem::take(&mut scan.files) {
        let listed = path.to_string_lossy();
        let name = listed.strip_prefix("./").unwrap_or(&listed).to_string();
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > MAX_FILE => continue,
            Ok(_) => {}
            Err(e) => {
                scan.skip(&path, e);
                continue;
            }
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes).into_owned();
                if worth_indexing(&name, &text) {
                    index.insert(&name, text);
                }
            }
            Err(e) => scan.skip(&path, e),
        }
    }
    index
}

/// The hits as the model sees them: each path, then for each matching chunk,
/// in file order, its definition line and the few lines sharing the most
/// query terms, numbered for `read_file`, from the text that was indexed.
pub fn render(index: &Index, hits: &[Hit], query: &str) -> String {
    let q = recall::query(query);
    let mut out = String::new();
    for hit in hits {
        let _ = writeln!(out, "{}", hit.path);
        let Some(file) = index.files.get(&hit.path) else { continue };
        let lines: Vec<&str> = file.text.lines().collect();
        let mut spans = hit.spans.clone();
        spans.sort_unstable();
        for (start, end) in spans {
            let end = end.min(lines.len());
            if start >= end {
                continue;
            }
            let mut shown: Vec<usize> = Vec::new();
            if is_definition(lines[start]) {
                shown.push(start);
            }
            let mut scored: Vec<(usize, usize)> = (start..end)
                .filter(|i| !shown.contains(i))
                .map(|i| (recall::query(lines[i]).intersection(&q).count(), i))
                .filter(|&(n, _)| n > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            shown.extend(scored.into_iter().take(LINES_SHOWN).map(|(_, i)| i));
            shown.sort_unstable();
            for i in shown {
                // Control characters would reach the model and the terminal.
                let cut: String =
                    lines[i].trim_end().chars().filter(|c| *c == '\t' || !c.is_control()).take(160).collect();
                let _ = writeln!(out, "{:>5}: {cut}", i + 1);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(files: &[(&str, &str)]) -> Index {
        let mut ix = Index::default();
        for (p, t) in files {
            ix.insert(p, t.to_string());
        }
        ix
    }

    fn paths(hits: &[Hit]) -> Vec<&str> {
        hits.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn definitions_are_recognised() {
        assert!(is_definition("pub fn run() {"));
        assert!(is_definition("    def flush(self):"));
        assert!(is_definition("export async function load() {"));
        assert!(is_definition("    public void save(int x) {"));
        assert!(!is_definition("    private int count;"));
        assert!(!is_definition("let x = fn_call();"));
        assert!(!is_definition("                fn too_deep() {}"));
    }

    #[test]
    fn rare_shared_words_outrank_common_ones() {
        let ix = build(&[
            (
                "app/store/blob.py",
                "def sync_blob(entries):\n    tmp = path + '.tmp'\n    os.replace(tmp, path)  # atomic write to disk\n",
            ),
            ("app/net/client.py", "def fetch(url):\n    cache = {}\n    return cache.get(url)\n"),
            ("app/cache.py", "class Cache:\n    def get(self, key):\n        return self.cache[key]\n"),
        ]);
        let hits = ix.search("which function writes to disk atomically", 5);
        assert_eq!(paths(&hits)[0], "app/store/blob.py");
        assert_eq!(hits[0].spans, vec![(0, 3)]);
    }

    #[test]
    fn identifiers_meet_plain_words_and_paths_count() {
        let ix = build(&[
            ("src/billing/tax.rs", "pub fn compute(amount: u64) -> u64 {\n    amount * TAX_RATE / 100\n}\n"),
            ("src/shipping.rs", "pub fn compute(weight: u64) -> u64 {\n    weight * 3\n}\n"),
        ]);
        assert_eq!(paths(&ix.search("tax rate", 5)), ["src/billing/tax.rs"]);
        // The path alone carries "billing".
        assert_eq!(paths(&ix.search("billing compute", 5))[0], "src/billing/tax.rs");
        assert!(ix.search("zebra quokka", 5).is_empty());
    }

    #[test]
    fn chunks_split_at_definitions_and_long_bodies() {
        let mut text = String::from("use std::fs;\nfn alpha() {\n    one();\n}\nfn beta() {\n");
        for _ in 0..70 {
            text.push_str("    gamma_delta();\n");
        }
        text.push_str("}\n");
        let ix = build(&[("a.rs", &text)]);
        let spans: Vec<(usize, usize)> = ix.files["a.rs"].chunks.iter().map(|c| (c.start, c.end)).collect();
        assert_eq!(spans, vec![(0, 1), (1, 4), (4, 64), (64, 76)]);
    }

    #[test]
    fn render_shows_the_definition_and_best_lines_without_control_characters() {
        let ix = build(&[
            (
                "store.py",
                "def save_all(items):\n    log('start')\n    data = encode(items)\n    write_atomic(path, data)\n",
            ),
            ("late.py", "def save_late(items):\n    write_atomic(\x1b[31mpath\x1b[0m)\n"),
        ]);
        let hits = ix.search("write atomic", 5);
        let hit = |p: &str| {
            hits.iter()
                .filter(|h| h.path == p)
                .map(|h| Hit { path: h.path.clone(), spans: h.spans.clone() })
                .collect::<Vec<_>>()
        };
        let out = render(&ix, &hit("store.py"), "write atomic");
        assert_eq!(out, "store.py\n    1: def save_all(items):\n    4:     write_atomic(path, data)\n");
        let out = render(&ix, &hit("late.py"), "write atomic");
        assert!(out.contains("write_atomic([31mpath[0m)") && !out.contains('\x1b'), "{out:?}");
    }

    #[test]
    fn index_reads_the_scan_now_and_reports_files_it_could_not_read() {
        let dir = std::env::temp_dir().join(format!("rusty-locate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.py"), "def alpha_beta(): pass\n").unwrap();
        std::fs::write(dir.join(".env"), "SECRET_TOKEN=alpha_beta\n").unwrap();
        std::fs::write(dir.join("blob.bin"), b"alpha_beta\0\x01").unwrap();
        std::fs::write(dir.join("run.ansi"), "\x1b[1malpha_beta\x1b[0m\n").unwrap();
        std::fs::write(dir.join("gone.py"), "def alpha_beta_gone(): pass\n").unwrap();
        let mut scan = crate::workspace_query::files_under(&dir);
        // Deleted between listing and reading: skipped, and the scan says so.
        std::fs::remove_file(dir.join("gone.py")).unwrap();
        let ix = index(&mut scan);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(ix.len(), 1, "secrets, binaries and recordings stay out");
        let hits = ix.search("alpha beta", 5);
        assert!(hits.len() == 1 && hits[0].path.ends_with("/a.py"), "{hits:?}");
        assert!(scan.incomplete && scan.skipped == 1, "{:?}", scan.diagnostics);
        assert!(scan.diagnostics[0].contains("gone.py"));
    }
}
