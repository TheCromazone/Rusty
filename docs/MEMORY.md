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
  when the lesson was saved, 0.5 when one changed (the advice then names it
  and says to read its current version first and follow it where it
  differs), and 0 when one is missing; the lesson is kept, since another
  branch may still have the file. Blob ids are cached by size and mtime, so a
  hook only `stat`s files it has already hashed.
  A lesson the agent saved starts at confidence 0.36, below one the user
  vouched for, so at `learn` a changed file's 0.5 puts it under the threshold
  for the request alone. It is offered later in the session, with the
  warning, once the tool calls and error lines `learn` observes raise its
  relevance: in the changed-anchor test (VERIFICATION.md) at requests 5 to
  32. Offering it at once was tested and parked: no extra passes, and one
  candidate added a stale run.
  `freshness_by_source_unchanged_changed_and_missing_anchors` pins the
  behaviour for the request alone.

A lesson is offered once per turn, and from then on it stays in the system
prompt for the rest of that turn, so a step that needs it later still sees it.
A turn carries at most the 16 latest lessons offered in it.

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
- `evals/multi/memory-use-agent` and `memory-use-user` measure use alone.
  Before a single session, the same seed is saved straight into memory, by
  the agent or by the user. Rusty offers it at every level, so a miss with
  memory on means the model ignored a lesson it was shown. `--memory off`
  is the baseline, and fails by construction.
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
not change its result. The `memory-use` tasks measure use directly, and
come before any change to how anchors or advice work. Elsewhere in the matrix, memory neither clearly
helped nor hurt: `stale-anchor` and `compact-focus` passed at every level,
and `poisoned-lesson` and `compaction-rule` missed with memory off and at
most levels, with no consistent direction in three repeats.

**What the memory-use runs showed** (development evidence: nemotron-3-super,
main 14fba44, five repeats per level per task, October 2026). Every scored run
failed at both levels, mostly on the 600-second timeout, and four runs were
`leak` (they read another run's working copy while searching for the seed).
In all ten `learn` runs the lesson was offered once, and in no run of either
level did the seed string appear in a command. Every run named `STATS_SEED`
and then tried to recover the seed from the test's hash. The runs also showed
why the lesson went unused: the service offers a lesson once per turn, and the
advice was added to that one request's system prompt only. The seed was in
front of the model for a single request, while the model first looked for it
four or five steps later. The "use" failure in the matrix above is therefore at
least partly a delivery failure. An offered lesson now stays in the prompt for
the rest of the turn, and the trajectory records which request first carried
each one, so the next runs can tell the two apart.

**After the fix** (development evidence: nemotron-3-super, main ae99853, five
repeats per level per task, October 2026). Between 14fba44 and ae99853 three
things changed together: an offered lesson stays in the prompt for the whole
turn, each session is isolated (`EVAL_ISOLATE=1`), and the cap rose from 600 to
1800 seconds. Per cell, as pass / fail / timeout / leak:

| | memory-use-agent | memory-use-user |
|---|---|---|
| `off` | 0 / 4 / 0 / 1 | 0 / 3 / 1 / 1 |
| `learn` | 5 / 0 / 0 / 0 | 5 / 0 / 0 / 0 |

In every `learn` run the lesson was offered with the first request and the seed
was first used at request 5 to 7, where the earlier delivery had already dropped
it; each run took 11 to 21 seconds. The trajectories separate the delivery change
from the other two: the cap cannot matter to a run that ends in 21 seconds, and
isolation only removes places to find the seed. In no `off` run did the seed
appear in a command; the scored runs spent 7 to 50 commands trying to recover it
from the hash. Neither `leak` run saw the seed: one read its own home, the other
typed part of its own working copy's name. Both runs, at 14fba44 and ae99853,
were scored under the original leak rule and are not rescored.

What this does not show: that memory captures the lesson by itself (these tasks
seed it directly), that a delivered lesson helps on tasks other than these two,
or that it holds beyond one model at five repeats. The lesson text was written
with the task, not by the model under test.

**Capture and delivery** (development evidence: nemotron-3-super, main
1bffef1, isolated, 1800-second cap, October 2026). In `memory-only-fact`,
session 1 reads the onboarding note and is asked to save how to run the
suite; the reset deletes the note. At `learn`, session 1 saved the seed
command in all ten runs. Whether session 2 got it depended only on the
lesson's anchor. Each cell is pass / fail / timeout / leak:

| | runs | lesson offered in session 2 | result |
|---|---|---|---|
| `learn`, lesson not anchored | 6 | 6, at the first request | 6 / 0 / 0 / 0 |
| `learn`, lesson anchored to the note | 4 | 0 | 0 / 2 / 0 / 2 |
| `off` | 5 | – | 0 / 3 / 1 / 1 |

The memory-off runs predate the change that stopped offering memory tools
with memory off, so each spent 5 to 16 calls on `remember` and `recall`
that answered "memory is off". `memory-handoff` passed 5/5 at `learn`. So
capture works, and a delivered lesson is used; what is lost is a lesson
anchored to a file the next checkout no longer has. Keeping such a lesson
with a "(gone)" warning delivered it, but the model then acted on the
remembered value before checking it when the value had changed, which
failed the predeclared harm bar; the change is parked (VERIFICATION,
"Predeclared: a lesson whose anchor is gone").

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
- Durability: in WAL mode the store syncs to disk at checkpoints, not on every
  commit (`synchronous=NORMAL`). A power loss or OS crash can drop the last
  lessons saved before it, but never corrupts the store; a rusty or daemon
  crash loses nothing. If WAL is unavailable the store syncs every commit.
  Syncing every commit made an explicit save wait on the disk, and one slow
  sync could overrun a management call's one-second budget.

Socket, files and directories use 0600/0600/0700, and persistence refuses
symlinks. One redactor covers hooks, saves, imports, session and trajectory
JSON, history and audit metadata; imports containing recognizable credentials
are rejected. It covers known token shapes, credential fields and configured
secret values, not arbitrary or encoded secrets. Files are access-controlled,
not encrypted. Secure deletion and WAL truncation reduce retained bytes but
don't erase swap, snapshots, backups or exported files.

`/memory` lists lessons with confidence, outcome counts, source and anchored
files; `/memory status` shows counts for the project. `--stats` and
`--trajectory` include hook counts, missed deadlines, injections (lessons
offered, once per lesson per turn) and hook time, and `offered`: each lesson
offered, with the index in `usage` of the first request that carried it.

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
