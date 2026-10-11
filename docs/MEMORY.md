# Memory

`rusty-memoryd` is a small local daemon beside `rusty`, started on demand over
a Unix socket (macOS and Linux). It holds lessons in SQLite and decides which
one, if any, is worth showing the model. It never runs tools and never calls a
model; the one model request memory makes goes through the agent's own client
and budget. No Python, vector database or downloaded weights are needed.

```sh
rusty --memory learn --goal "fix the failing tests" --verify "make check"
rusty --memory recall "where do we deploy from?"
rusty --memory off "fix this small bug"
```

**Benchmark runs should use `--memory off`** (or `RUSTY_MEMORY=off`) unless
memory is what's being measured. Otherwise the default moves a run's cohort,
and lessons from one trial could reach the next. To measure memory, compare
pinned levels side by side.

## Levels

Each level includes the ones above it. Higher levels capture more, offer
lessons more readily and prune harder, since more capture means more noise.

| Level | What it adds | Offered when score ≥ / per turn | Retired when |
|---|---|---|---|
| `off` | No memory. | – | – |
| `recall` | Offers lessons you or the agent saved that match the request. Your preferences are added to every prompt. | 0.25 / 1 | an unvouched lesson has had no success in 28 days |
| `learn` (default) | The last four tool calls (arguments and error lines) join the request. Lessons record the files they describe and are re-checked before use. A goal's `--verify` check credits or charges the lessons it was offered. | 0.15 / 2 | mean < 0.3 after 2+ outcomes, or 28 days |
| `reflect` | After a checked goal, one budgeted request says which offered lessons the work used and, only after a pass, keeps up to two short new lessons. Compaction's lessons are kept too. | 0.15 / 2 | mean < 0.35, or 21 days |
| `deep` | Up to three new lessons per pass and more offered per turn. | 0.10 / 3 | mean < 0.4, or 14 days |

## How a lesson is chosen

Each lesson is scored **relevance × confidence × freshness**, and the best one
above the level's threshold is offered as one short line.

- **Relevance** is BM25 (k1 1.2, b 0.75) over an inverted index. Identifiers
  are kept whole and split into their parts (`parseConfig`, `gen_fixtures`,
  `src/tax_rate.rs`), so "tax rate" finds `taxRate`; plain words are lightly
  stemmed ("tests" meets "test", "rounding" meets "round"). A lesson's anchored
  paths count twice. The IDF is smoothed, ln((1+N)/(1+df)) + 1, so a word every
  lesson shares still counts once; Okapi's own IDF drops it to nearly zero,
  which leaves a store of similar lessons unable to match anything. The score
  maps to 0-1 as s / (s + 2): one shared everyday word gives 0.33, three give
  0.6. A search touches only the lessons that share a term with the query.
- **Confidence** is a Beta posterior over outcomes, its mean less half a
  standard deviation. You saving a lesson counts as three successes and a
  failure; the agent, compaction or reflection count as one of each. Being
  shown never changes it. A lesson the agent saved starts at 0.36, reaches
  0.55 after one verified success and drops to 0.21 after one failure.
- **Freshness** is 1 when every anchored file still has the git blob id it had
  when the lesson was saved, 0.5 when one changed (the advice then says which
  to re-check), and 0 when one is missing; the lesson is kept, since another
  branch may still have the file. Blob ids are cached by size and mtime, so a
  hook only `stat`s files it has already hashed.

Preferences are not ranked: they apply everywhere, so `begin` returns them
(this project's and global ones, `rusty-memoryd remember --global`) for the
system prompt every turn, and they never crowd lessons out of the advice slots.

## How memory learns and forgets

- **Credit is narrow.** Only a goal's fixed check counts. A pass credits the
  lessons the goal was offered (with `reflect`, only those the model says the
  work relied on); a fail charges the same set. A timeout or interruption
  teaches nothing, and without `--verify` nothing is credited: a model saying
  it finished is not evidence. `/memory helpful` and `/memory harmful` count as
  two outcomes.
- **New lessons start as candidates.** Reflection may only add lessons after a
  pass, each one sentence under 200 characters naming its files; the parser
  drops anything else, including invented preferences.
- **Near-duplicates merge.** A new lesson whose terms overlap an existing one
  of the same kind by 80% or more refreshes it instead of splitting its
  outcomes across copies.
- **Retirement** deletes and tombstones a lesson, so an import can't bring it
  back. It runs in SQL when a project's lessons are loaded and after credit.
  Only a success restarts the expiry clock; your own lessons never expire.
- **A full store** (512 lessons) evicts the least trusted lesson nobody
  vouched for. Eviction is about space, so it leaves no tombstone.

## Why these choices

- Adding every experience makes agents worse, because they copy retrieved
  records, mistakes included; strict selective addition plus deletion of
  low-utility records gave about +10% over naive growth
  ([Xiong et al., ACL 2026](https://arxiv.org/abs/2505.16067)).
- Itemised lessons with helpful and harmful counters, refined in place, beat
  rewriting a summary ([ACE, ICLR 2026](https://arxiv.org/abs/2510.04618);
  [ExpeL](https://arxiv.org/abs/2308.10144)).
- On real repository tasks, short procedural lessons anchored to the files
  they fix help a little; long transcripts and general-purpose memory systems
  mostly hurt, and irrelevant text costs a few points
  ([VibeMemBench](https://arxiv.org/abs/2609.23570);
  [SWE-ContextBench](https://arxiv.org/abs/2602.08316)). Hence one short line at
  a time, and a threshold below which nothing is offered.
- GitHub's Copilot Memory checks each memory's code citations before use and
  drops memories unused for 28 days
  ([docs](https://docs.github.com/en/copilot/concepts/agents/copilot-memory)).
- Keeping identifiers whole and split roughly doubled BM25's nDCG@10 on a code
  retrieval set ([arXiv 2605.18561](https://arxiv.org/abs/2605.18561), preprint).

**What BM25 can't do.** Lessons and queries here are short and full of exact
names, paths, commands and error text, which is where lexical matching is
strongest. It cannot bridge vocabulary: "tests" never meets `pytest`, nor
"rounding" `quantize`. Stemming narrows that a little, anchored paths match
the files a task touches, and the agent can search in its own words with the
`recall` tool. Dense embeddings would close more of the gap but need
downloaded weights; revisit them if evals show missed recalls.

The constants (priors, the half deviation, thresholds, 0.3-0.4, 14-28 days)
are engineering defaults, **not fitted values**. Compare levels with
`EVAL_MEMORY="off recall learn reflect deep"` and repeats. The multi-session
memory tasks test different things:

- `evals/multi/memory-only-fact` is the only one that can show memory
  helping. Session 1 reads a test seed from an onboarding note. The workspace
  is then reset to a fresh copy without the note, and only session 2's own
  run of the suite is graded. Without memory it can't pass, except by
  reading the harness's records: session 1's transcript or Rusty's own
  session history in the run's home. The eval scores such a run `leak` and
  doesn't count it.
- `evals/multi/memory-handoff` grades whether a lesson was saved, so
  `--memory off` fails it by construction. Its second session can find the
  test command in the Makefile, so a pass shows the mechanism works, not
  that memory helped.
- `evals/multi/stale-anchor` and `evals/multi/poisoned-lesson` test harm:
  - in `stale-anchor`, a project's prices move between sessions, so a
    remembered location goes stale;
  - in `poisoned-lesson`, a wrong rounding rule is planted that memory
    should stop offering once checks fail.

  Both can be solved without memory.

**What a first matrix showed** (development evidence, not a result:
nemotron-3-super, main 74d40a2, three repeats per level, October 2026). No
level passed `memory-only-fact`. Its one pass, and two other runs, got or
looked for the seed outside memory: in the run's home, in another run's
directory, and in a note a memory-off run had written to `/tmp`. The eval
now scores all three `leak`. Watching the memory store during six runs
separated two failures:

- **Delivery.** In three of the six, the model listed `ONBOARDING.md` in the
  lesson's `files`, since the tool asks for "files the memory is about". The
  reset deletes that note, so freshness was 0 and session 2 was never
  offered the lesson. A fact anchored to where it was read, rather than to
  what it describes, disappears with its source.
- **Use.** In the other three the lesson was offered, and session 2 never
  used it: it tried to recover the seed from the test's hash instead, and
  ran out of time.

Since this model didn't use an offered lesson, fixing delivery alone would
not change its result. The next step is to measure use directly, by seeding
the lesson (from the user and from the agent) into a fresh session 2 and
checking whether it runs the suite with the seed, before changing how
anchors or advice work. Elsewhere in the matrix, memory neither clearly
helped nor hurt: `stale-anchor` and `compact-focus` passed at every level,
and `poisoned-lesson` and `compaction-rule` missed with memory off and at
most levels, with no consistent direction in three repeats.

## Bounded hot path and disk

- Sessions: RAM only, at most 32, each with a 4 KiB request and the last four
  observations (1 KiB arguments, 2 KiB output). Hooks never write to disk; a
  daemon restart loses sessions but keeps lessons.
- Hooks: a queue of eight; the caller waits at most 50 ms, and the transport
  times out at 200 ms so abandoned work drains. Management calls get a second.
- Lessons: at most 512 across the store, plain TEXT rows indexed by project.
  SQLite has a 16 MiB page quota, a 512 KiB page cache, in-memory temporary
  tables and frequent WAL checkpoints. A store from an older schema is dropped
  and recreated, not migrated.

Socket, files and directories use 0600/0600/0700, and persistence refuses
symlinks. One redactor covers hooks, saves, imports, session and trajectory
JSON, history and audit metadata; imports containing recognizable credentials
are rejected. It covers known token shapes, credential fields and configured
secret values, not arbitrary or encoded secrets. Files are access-controlled,
not encrypted. Secure deletion and WAL truncation reduce retained bytes but
don't erase swap, snapshots, backups or exported files.

`/memory` lists lessons with confidence, outcome counts, source and anchored
files; `/memory status` shows counts for the project. `--stats` and
`--trajectory` include hook counts, missed deadlines, injections and hook time.

## Local operation and cloud transfer

Keep the same `RUSTY_PROJECT_ID` locally and in the cloud. Without one,
worktrees sharing a Git common directory share memory; unrelated clones do
not. Automatically started daemons have their own process group, so Ctrl-C in
a coding turn doesn't stop memory. `--home` / `RUSTY_HOME` isolates a store.

```sh
export RUSTY_PROJECT_ID=https://github.com/you/project
rusty-memoryd remember --kind fact "The monorepo URL is https://github.com/you/project"
rusty-memoryd remember --kind preference --global "Answer in British English"
rusty-memoryd list
rusty-memoryd export project-memory.json.gz
rusty-cloud run --repo "$RUSTY_PROJECT_ID" --memory-mode learn \
  --memory-input project-memory.json.gz --project-id "$RUSTY_PROJECT_ID" \
  --goal "fix the failing tests"
```

Exports are scoped gzip JSON through the live service, never copies of an
active database, and are created without replacing an existing file. Imports
need the same project and schema version; outcomes don't travel, so an
imported lesson earns trust again. The Daytona launcher uploads an optional
memory snapshot, runs the daemon inside the same sandbox as the coding
process, and exports `memory.json.gz` with the results.

## Checks

`cargo xtask qa` includes the service tests and an offline smoke through the
real CLI at every level: file edits, Bash, a preserved sentinel, lesson
injection, memory off, hook deadlines and tool budgets.
`cargo xtask memory-smoke --live` runs the same cases on a hosted model, and
`memory-live.yml` runs them only when dispatched by hand.
`cargo xtask memory-bench` fills a store with 512 lessons and times 100 advice
calls over the real socket; it passes when p95 is under 20 ms and storage
under 20 MiB. None of this shows that memory improves hard tasks; establish
that with paired, repeated trials across levels.
