# Finding code

Rusty finds code with four tools. `search` (ripgrep) finds exact text, `glob`
finds paths, and `outline` lists definitions. `locate` finds code from a
description when you don't know the names to grep for. This page explains
`locate` and records what was measured before it was added: whether a ranked
index finds the right files more often than grep, and whether that changes
what an agent does.

## What `locate` does

```
locate "retry when the stream drops"
src/llm.rs
  470:     /// Runs one streamed request, and runs it again when the provider reports
  472:     /// whose events carry the error) or the stream drops, as long as nothing
  473:     /// has been shown yet. Once answer text has streamed, a retry would repeat
  475:     fn stream_with_retry(
  481:         let budget = retry_budget();
  488:                 // verbose view, so a drop while the model thinks is retryable.
  493:                 Err(e) if !shown && transient_stream_error(&format!("{e:#}")) => e,
…and the next files, up to eight
```

- **Ranking.** BM25 is scored twice. Once over chunks, each running from one
  definition to the next (at most 60 lines), with the file's path words
  counted in every chunk. Once over whole files. Reciprocal rank fusion
  (k = 60) merges the two rankings.
- **Words.** Identifiers are split as in memory's recall
  ([MEMORY.md](MEMORY.md)), so "tax rate" meets `taxRate` and plain words are
  lightly stemmed.
- **Output.** At most eight files. Each shows up to two chunks, in file order:
  the definition line and the three lines that share the most query words,
  numbered for `read_file`. That's about 700 to 950 tokens.
- **Nothing is kept between calls.** Each call lists the files a `search`
  would read (ripgrep's hidden and ignore rules, through the bounded query
  layer in [WORKSPACE_QUERIES.md](WORKSPACE_QUERIES.md)), reads them,
  indexes them, ranks, and drops the index. Lines are shown from that same
  reading. Unreadable files and cut-short listings are reported in the usual
  `[query scan_complete=false …]` header. Without ripgrep, the listing falls
  back to glob's rules and the header says so.
- **Never indexed:** `.env*`, `*.pem`, `*.key`, lock files, files over 1 MB,
  binary files and minified text. The output also passes through the same
  redaction as every other tool.

### Why no Merkle tree, and no cache

Cursor's index is kept fresh with a Merkle tree. Each file is hashed, each
directory is hashed from its children, and comparing root hashes with the
server finds the changed files without walking everything. That pays off
when a long-lived index on another machine has to be kept in step over a
network. Rusty has no long-lived index to keep in step.

An earlier version of `locate` did keep one in memory, rechecked by each
file's size and modification time the way git's index is. It was dropped.
The other workspace queries keep no cache until measured reuse justifies
the freshness risk, and a stamp check misses an edit that keeps a file's
size within one timestamp tick. A cold call costs well under a second on
repositories of a few thousand files, which is what Daytona's tool bridge
paid on every call anyway. If agents turn out to call `locate` many times
per session, a cache can come back with the hit, miss and validation
receipts that WORKSPACE_QUERIES.md asks for.

### Why no embeddings

Embeddings would need downloaded weights or a network service, which rusty
avoids. On code, lexical search with identifiers kept whole and split is
competitive ([arXiv 2605.18561](https://arxiv.org/abs/2605.18561)). Where it
can't bridge vocabulary ("persist" against `flush_to_disk`), the model can
rephrase and search again.

### What it costs

It adds about 80 KB to the release binary. Each call does all the work
again, so its cost is a cold call's: listing, reading, indexing and
searching. CPU time per call through `--tool-rpc`, steady over three runs
(wall time on a busy machine was higher):

| Repository | Files listed | CPU per call |
|---|---|---|
| rusty | 261 | 0.1 s |
| just-bash (TypeScript) | 1,837 | 0.5 s |
| harbor (Python) | 3,503 | 1.2 s |
| wasmtime (Rust) | 7,479 | 2.2 s |

Peak memory was 12 MB on rusty and 107 MB on wasmtime, freed when the call
returns.

## Does it find the right files?

`cargo run --release --example locate_bench -- REPO [QUERIES] [--scope]`
replays a repository's history:

- **Queries** come from commits spread evenly over the last 4,000. A commit
  qualifies if it modified one to eight text files and its subject has at
  least three terms.
- **The query** is the subject, with `type(scope):`, `[tag]` and `(#123)`
  removed unless `--scope` is passed.
- **The corpus** is the parent's tree, read from git objects, so the fix
  itself never leaks in.
- **The answer** is the set of files the commit modified.

Two greps stand in for an agent without `locate`:

- **grep-rarest:** one search for the query word found in the fewest files,
  with files in path order. A first grep usually goes like this.
- **grep-all:** every query word searched, with files ranked by how many
  distinct words they contain. This is stronger than what most agents run.

Mean reciprocal rank, how often a changed file came first, and precision@5
(the share of the top five that were changed files; a perfect ranking scores
45% to 57% here, since most commits change only one to three files):

| Repository | Files | Scope | grep-rarest MRR | grep-all MRR | locate MRR | hit@1 grep-all → locate | precision@5 grep-all → locate |
|---|---|---|---|---|---|---|---|
| rusty | 178 | removed | 0.33 | 0.52 | **0.69** | 34% → 59% | 27% → 34% |
| rusty | 178 | kept | 0.32 | 0.52 | **0.74** | 32% → 64% | 28% → 37% |
| agent-browser | 201 | removed | 0.27 | 0.49 | **0.56** | 31% → 46% | 24% → 23% |
| agent-browser | 180 | kept | 0.17 | 0.39 | **0.41** | 21% → 26% | 20% → 17% |
| just-bash | 1,495 | removed | 0.14 | 0.23 | **0.37** | 8% → 25% | 11% → 17% |
| just-bash | 1,439 | kept | 0.17 | 0.29 | **0.42** | 17% → 30% | 10% → 17% |
| harbor | 1,886 | removed | 0.14 | 0.26 | **0.46** | 17% → 35% | 10% → 17% |
| harbor | 1,929 | kept | 0.07 | 0.24 | **0.42** | 14% → 32% | 10% → 17% |

At 1,500 to 2,000 files, `locate` put a changed file first two to three
times as often as grep-all, and its top five held fewer wrong files. At 200
files it still led at rank one, but grep-all's top five was as good or
slightly better (agent-browser: a changed file in 73% against 71% with scope
removed and 68% against 60% with it kept, and a little more precise). Against
the single grep an agent usually starts with, it wins everywhere.

A prototype tried other variants first, over the same repositories and
settings. Its grep-all scored a mean MRR of 0.39, whole files alone 0.45,
and fusion 0.49. Files alone fell well behind on the small repositories
(agent-browser: 0.41 against 0.57 for chunks). Chunks with path words alone
also scored 0.49 and stayed within 0.06 of fusion everywhere, either side,
so the two are a tie; fusion is what the agent runs below measured. A
linear blend of scores tied too. Two variants were dropped outright: plain
words without identifier splitting, and adding the files that most often
changed in the same commits as the top hit.

## Does it help an agent?

This study was exploratory, not predeclared (compare
[VERIFICATION.md](VERIFICATION.md)).

**Setup.** Each task is a real commit from harbor or just-bash. The agent
gets the parent's tracked files with no `.git`, and this prompt: *A teammate
asks for this change: "<subject>". Which existing files would need to change?
Write the paths to ANSWER.txt, most likely first.* Answers are scored against
the files the commit modified.

**Two samples.** One has 16 commits spread evenly. The other has 16
"hard-for-grep" commits, where grep-all ranked no changed file in its top
five. That subset was chosen by grep's failure alone, never by `locate`'s
success.

**What was compared.** Four arms, each built from the same main (`ad3f8aa`):

- **base:** main as it was.
- **offered:** the `locate` tool, with no change to the instructions.
- **guided:** the tool, plus the instruction this branch ships: "When you
  don't know where something lives, start with locate."
- **hint:** the top five files listed in the prompt each turn.

**Models.** `gpt-6-astra` through the Codex sign-in, and Nemotron 3 Super
through NVIDIA. Memory was off. The runs used an earlier build that kept its
index between calls. Each run called `locate` about once, so the cache never
came into play.

Codex, 16 even tasks:

| Arm | hit@1 | Recall | Mean tokens | Median tokens | Requests | Runs calling locate |
|---|---|---|---|---|---|---|
| base | 94% | 89% | 125k | 123k | 13.3 | – |
| offered | 94% | 82% | 117k | 88k | 12.8 | 0% |
| guided | 100% | 94% | 117k | 91k | 13.5 | 88% |
| hint | 94% | 81% | 105k | 64k | 11.8 | – |

Codex, 16 hard-for-grep tasks:

| Arm | hit@1 | Recall | Mean tokens | Median tokens | Requests | Runs calling locate |
|---|---|---|---|---|---|---|
| base | 100% | 92% | 176k | 170k | 15.8 | – |
| guided | 100% | 91% | 138k | 137k | 14.2 | 100% |
| hint | 100% | 94% | 191k | 187k | 16.9 | – |

Paired by task, for the guided arm against base:

- **Hard tasks:** guided was cheaper on 13 of 16 (geometric mean of the token
  ratio 0.79, 1.6 fewer requests). A sign test gives p ≈ 0.01.
- **Even tasks:** cheaper on 7 of 16 (0.98).
- **Accuracy:** recall moved by less than run-to-run noise. The offered arm,
  which never called the tool and so behaved like base, still moved 0.07.
- **What changed:** each run made one `locate` call, about one fewer grep, and
  about 0.7 more file reads to check the hits.

The hint arm saved tokens on the even tasks (0.85) and cost more on the hard
ones (1.07).

Nemotron ran under heavy rate limiting: 8 of 21 runs were lost to it.
**It never called `locate`, with or without the instruction.** The hint arm
used about half the tokens on the three tasks that paired cleanly, too few
to act on.

### What this means

- **A strong model's own grep loop already makes up for grep's ranking.**
  Codex scored 94% to 100% on top-file accuracy without `locate`, including on
  the commits grep ranked worst.
- **`locate` pays when grep is weak and the model is told when to use it.**
  That's about a fifth fewer tokens at the same accuracy. The instruction is
  needed: offered alone, the tool went unused.
- **A tool doesn't help a model that won't call it.** For Nemotron, the hint
  is the more promising route.

### Open

- A predeclared test of the per-turn hint for models that don't call
  `locate`, with enough Nemotron runs to read.
- Line-level precision: whether the lines `locate` shows are the lines a
  change touches.
- Large repositories, where every call costs seconds of CPU (2.2 s at 7,500
  files). A cache would first need sessions that call `locate` often.
