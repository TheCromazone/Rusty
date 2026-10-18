//! Lexical retrieval for the memory advisor: an identifier-aware tokenizer
//! and a BM25 inverted index.
//!
//! Lessons and queries are short and code-heavy: `parseConfig`,
//! `gen_fixtures`, `src/billing/tax.rs`, an error line. Each identifier is
//! kept whole (exact-name matches) and also split into its camelCase,
//! snake_case and path parts, so "tax rate" finds `taxRate`. Emitting both,
//! rather than parts alone, is what made BM25 competitive on code retrieval
//! in arXiv 2605.18561. Plain words get a light suffix stemmer so "tests"
//! meets "test" and "rounding" meets "round". BM25 still can't bridge
//! synonyms ("tests" and `pytest`); the agent's own `recall` searches cover
//! that gap.

use std::collections::{HashMap, HashSet};
const K1: f64 = 1.2;
const B: f64 = 0.75;

const STOP: &[&str] = &[
    "the", "and", "for", "that", "this", "with", "you", "are", "was", "but", "not", "have", "has", "can", "from",
    "into", "use", "will", "should", "when", "what", "how", "why", "all", "any", "our", "its", "then", "than", "also",
    "just", "please", "make", "let", "lets", "get", "them", "they", "there", "here", "out", "per", "via", "such", "is",
    "it", "to", "of", "in", "on", "an", "be", "or", "as", "at", "by", "if", "so", "do", "we", "me", "my", "no", "up",
    "us",
];

/// Splits `HTTPServerError2` into `http`, `server`, `error2`.
fn camel_parts(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();
    let mut parts = Vec::new();
    let mut start = 0;
    for i in 1..chars.len() {
        let (prev, cur) = (chars[i - 1], chars[i]);
        let next_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
        if (prev.is_lowercase() && cur.is_uppercase()) || (prev.is_uppercase() && cur.is_uppercase() && next_lower) {
            parts.push(chars[start..i].iter().collect::<String>().to_lowercase());
            start = i;
        }
    }
    parts.push(chars[start..].iter().collect::<String>().to_lowercase());
    parts
}

/// Strips common English inflections from a lowercase ASCII word. It only
/// has to be consistent, since queries and lessons go through it alike.
fn stem(word: &str) -> String {
    if !word.bytes().all(|b| b.is_ascii_lowercase()) || word.len() < 4 {
        return word.to_string();
    }
    let undouble = |s: &str| {
        let b = s.as_bytes();
        let n = b.len();
        if n > 2 && b[n - 1] == b[n - 2] && !b"aeioulsz".contains(&b[n - 1]) {
            s[..n - 1].to_string()
        } else {
            s.to_string()
        }
    };
    if let Some(s) = word.strip_suffix("ies").filter(|s| s.len() > 1) {
        return format!("{s}y");
    }
    // As in Porter's stemmer, the stem must keep a vowel: "string" stays.
    let vowel = |s: &&str| s.len() > 2 && s.bytes().any(|b| b"aeiouy".contains(&b));
    if let Some(s) = word.strip_suffix("ing").filter(vowel) {
        return undouble(s);
    }
    if let Some(s) = word.strip_suffix("ed").filter(vowel) {
        return undouble(s);
    }
    if let Some(s) = word.strip_suffix("es").filter(|s| ["s", "x", "z", "ch", "sh"].iter().any(|e| s.ends_with(e))) {
        return s.to_string();
    }
    match word.strip_suffix('s') {
        Some(s) if !s.ends_with(['s', 'u', 'i']) => s.to_string(),
        _ => word.to_string(),
    }
}

/// Index terms for `text`: each identifier lowercased whole, plus its parts
/// when it has more than one, with plain words stemmed. Stopwords and
/// one-letter tokens are dropped.
pub fn terms(text: &str) -> Vec<String> {
    let keep = |t: &str| t.chars().count() > 1 && !STOP.contains(&t);
    let mut out = Vec::new();
    // Reused across identifiers, so only the kept terms allocate.
    let (mut whole, mut part, mut parts) = (String::new(), String::new(), Vec::new());
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if !raw.is_ascii() {
            unicode_terms(raw, &keep, &mut out);
            continue;
        }
        whole.clear();
        whole.push_str(raw.trim_matches('_'));
        if whole.is_empty() {
            continue;
        }
        whole.make_ascii_lowercase();
        parts.clear();
        for piece in raw.split('_').filter(|p| !p.is_empty()) {
            ascii_camel_parts(piece, &mut parts);
        }
        if keep(&whole) {
            out.push(stem(&whole));
        }
        if parts.len() > 1 {
            for p in &parts {
                part.clear();
                part.push_str(p);
                part.make_ascii_lowercase();
                if keep(&part) && part != whole {
                    out.push(stem(&part));
                }
            }
        }
    }
    out
}

/// [`camel_parts`] for ASCII, as slices of `word` (still in their case).
fn ascii_camel_parts<'a>(word: &'a str, parts: &mut Vec<&'a str>) {
    let b = word.as_bytes();
    let mut start = 0;
    for i in 1..b.len() {
        let (prev, cur) = (b[i - 1], b[i]);
        let next_lower = b.get(i + 1).is_some_and(u8::is_ascii_lowercase);
        if (prev.is_ascii_lowercase() && cur.is_ascii_uppercase())
            || (prev.is_ascii_uppercase() && cur.is_ascii_uppercase() && next_lower)
        {
            parts.push(&word[start..i]);
            start = i;
        }
    }
    parts.push(&word[start..]);
}

/// The general path of [`terms`] for one identifier with non-ASCII letters.
fn unicode_terms(raw: &str, keep: &dyn Fn(&str) -> bool, out: &mut Vec<String>) {
    let whole = raw.trim_matches('_').to_lowercase();
    if whole.is_empty() {
        return;
    }
    let parts: Vec<String> = raw.split('_').filter(|p| !p.is_empty()).flat_map(camel_parts).collect();
    if keep(&whole) {
        out.push(stem(&whole));
    }
    if parts.len() > 1 {
        out.extend(parts.into_iter().filter(|p| keep(p) && *p != whole).map(|p| stem(&p)));
    }
}

/// The distinct terms of `text`, as a query.
pub fn query(text: &str) -> HashSet<String> {
    terms(text).into_iter().collect()
}

/// BM25 over a small set of documents, as an inverted index. Each posting
/// holds its finished term weight, so a search only touches the documents
/// that share a term with the query.
pub struct Index {
    postings: HashMap<String, Vec<(usize, f64)>>,
    len: usize,
}

impl Index {
    pub fn new(docs: &[Vec<String>]) -> Self {
        let mut postings: HashMap<String, Vec<(usize, f64)>> = HashMap::new();
        // Each document's term counts come from sorting its terms; only a
        // term the index has not seen yet is copied.
        let mut sorted: Vec<&str> = Vec::new();
        for (i, d) in docs.iter().enumerate() {
            sorted.clear();
            sorted.extend(d.iter().map(String::as_str));
            sorted.sort_unstable();
            for run in sorted.chunk_by(|a, b| a == b) {
                let posting = (i, run.len() as f64);
                match postings.get_mut(run[0]) {
                    Some(list) => list.push(posting),
                    None => drop(postings.insert(run[0].to_string(), vec![posting])),
                }
            }
        }
        let lens: Vec<f64> = docs.iter().map(|d| d.len() as f64).collect();
        let avg = (lens.iter().sum::<f64>() / lens.len().max(1) as f64).max(1.0);
        let n = docs.len() as f64;
        for list in postings.values_mut() {
            // Smoothed IDF, ln((1 + N) / (1 + df)) + 1, as scikit-learn's
            // TF-IDF uses: a term every lesson shares still counts once.
            // Okapi's IDF falls to nearly zero there, so a store of
            // similar lessons could never match anything.
            let idf = ((1.0 + n) / (1.0 + list.len() as f64)).ln() + 1.0;
            for (d, tf) in list.iter_mut() {
                *tf = idf * *tf * (K1 + 1.0) / (*tf + K1 * (1.0 - B + B * lens[*d] / avg));
            }
        }
        Self { postings, len: docs.len() }
    }

    /// Every document's score for `query` (zero for those sharing no term).
    pub fn scores(&self, query: &HashSet<String>) -> Vec<f64> {
        let mut out = vec![0.0; self.len];
        for term in query {
            for (doc, w) in self.postings.get(term).into_iter().flatten() {
                out[*doc] += w;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str) -> HashSet<String> {
        query(text)
    }

    #[test]
    fn identifiers_are_kept_whole_and_split() {
        assert_eq!(terms("parseConfig"), ["parseconfig", "parse", "config"]);
        assert_eq!(terms("gen_fixtures.py"), ["gen_fixtures", "gen", "fixture", "py"]);
        assert_eq!(terms("HTTPServer"), ["httpserver", "http", "server"]);
        assert_eq!(terms("src/billing/tax_rate.rs"), ["src", "bill", "tax_rate", "tax", "rate", "rs"]);
        // Stopwords and single letters go; plain words are stemmed.
        assert_eq!(terms("Run the tests with make check, a must"), ["run", "test", "check", "must"]);
        assert_eq!(terms("__init__"), ["init"]);
    }

    #[test]
    fn stemming_joins_inflections_without_mangling_code() {
        for (a, b) in [
            ("tests", "test"),
            ("rounding", "round"),
            ("rounded", "round"),
            ("running", "run"),
            ("stopped", "stop"),
            ("fixes", "fix"),
            ("matches", "match"),
            ("dependencies", "dependency"),
        ] {
            assert_eq!(stem(a), b);
        }
        for same in ["class", "status", "analysis", "pytest", "go", "tsx", "v2", "readme", "string", "thing"] {
            assert_eq!(stem(same), same);
        }
    }

    #[test]
    fn search_touches_only_matching_documents() {
        let docs = vec![terms("taxRate rounds half up in billing"), terms("the cache evicts oldest entries")];
        let index = Index::new(&docs);
        let s = index.scores(&q("fix the tax rate rounding"));
        assert!(s[0] > 0.0);
        assert_eq!(s[1], 0.0);
    }

    #[test]
    fn rare_terms_weigh_more() {
        let docs = vec![terms("tests run with make check"), terms("tests need the fixtures generated")];
        let index = Index::new(&docs);
        // "fixtures" is in one document, "tests" in both.
        assert!(index.scores(&q("fixtures"))[1] > index.scores(&q("tests"))[1]);
        // A term every document shares still counts.
        assert!(index.scores(&q("tests"))[0] > 0.5);
    }

    #[test]
    fn a_term_counts_more_in_a_larger_store() {
        let lesson = terms("regenerate fixtures before running tests");
        let one = Index::new(std::slice::from_ref(&lesson));
        let mut docs = vec![lesson];
        docs.extend((0..20).map(|i| terms(&format!("deployment note {i}"))));
        let many = Index::new(&docs);
        assert!(many.scores(&q("fixtures"))[0] > one.scores(&q("fixtures"))[0] * 2.0);
        assert_eq!(Index::new(&[]).scores(&q("anything")), Vec::<f64>::new());
    }
}
