# Design notes

Start with [How Rusty works](HOW_IT_WORKS.md) for the simple architecture diagrams
and feature walkthrough. This page gives the deeper design rationale.

rusty is meant to be the coding agent you'd keep open all day: quick for small
jobs, dependable over long ones, and honest about what it spends. This page
explains why it's built the way it is.

## What coding agents are good at today

- **Local edits with fast feedback.** Read a file, change it, run the test,
  fix the error. When the loop is short and the signal is clear, current
  models are excellent.
- **Finding things with terminal tools.** Most of the real progress in agents
  came from treating the shell as the interface: `grep`, `rg`, `find`, `git`,
  the build, the tests. These tools are fast and precise, and every model
  already knows them.
- **Reading unfamiliar code.** Given the right ten files, a model can explain a
  system faster than most people.

## Where they fall apart on long tasks

| Failure | What it looks like | What rusty does |
|---|---|---|
| **Context rot** | After an hour the agent forgets a constraint you set at the start, or redoes finished work. | A fixed token budget per section. Old tool output gets trimmed first, since it can be re-read, and only then are older turns summarised. Your messages are carried through every compaction **word for word**, and the plan and goal are pinned outside history. |
| **Lossy compaction** | The summary drops the task itself, and the agent asks "what would you like to do next?" | The same verbatim carry-over, plus compaction can cut mid-turn, so a single long autonomous turn can still be compacted (that bug is covered by a test). |
| **Premature "done"** | It says it's finished without running anything. | Goal mode only ends through `goal_done`, which requires evidence. Every other ending means "keep going", up to a turn cap. |
| **Doom loops** | The same failing command, eight times. | Repeated identical calls in one turn get a note telling the model to step back and change approach. |
| **Amnesia** | Every session relearns how to run the tests. | Typed memories ranked by BM25 against each request and injected within a budget. Compaction also pulls lasting facts out of the turns it summarises. |
| **Memory junk drawers** | A notes file that grows forever and gets pasted into every prompt. | Records, not a file. Near-duplicates merge, stale entries can be forgotten, and only the relevant few are injected. |
| **Permission fatigue** | Prompts for `ls` train people to approve everything, and then something risky slips through. | A classifier that reads what a command will really run (scripts, `make` targets, `package.json` scripts, `$(…)`, heredocs). Routine work and cloud reads just run, outward actions ask, and the short list of things you can't undo needs a typed yes every time, even in yolo. |
| **Exploration flooding context** | Fifty file reads to answer one question. | `outline` and `glob` show structure without file bodies, and subagents and swarms explore in their own context and return only a report. |
| **Invisible cost** | You find out what it spent when the bill arrives. | Per-turn recaps, `/tokens` broken down by model and by tool, per-request traces on disk, and tips based on what each turn actually spent. |
| **Hard to interrupt** | Ctrl-C kills the process or does nothing until the stream ends. | Requests run on a background thread, so Ctrl-C stops a turn mid-stream (about 1 ms in tests). The transcript stays valid and the next turn works. |

## Execution depth

How hard rusty works and what it's allowed to touch are separate settings.
`/mode` changes the first and `/permissions` the second, so "go fast"
never quietly means "go unguarded".

Careful mode doesn't trust the first "done". When a turn that changed
something ends, or a goal calls `goal_done`, one read-only worker gets the
task, your messages, the working set and the last few steps, and reviews
the result with fresh context. The main agent fixes what it finds and
verifies the fix. There's no second review, because a reviewer reviewing
the reviewer costs more than it finds. A plain question isn't reviewed,
and a goal is reviewed once however many turns it takes. The review comes
back marked as rusty's own, so compaction never carries it over as
something you said.

## First principles for tools

An agent's tools should be the ones a strong engineer reaches for in a
terminal, shaped so each call returns as little as possible while still
answering the question.

1. **Locate before you read.** `search` (ripgrep, smart case, bounded display,
   optional context lines and globs) answers "where", `glob` answers "which
   files", and `outline` answers "what's in here" with signatures and line
   numbers. `locate` answers "where is the code that does this" when the
   names are unknown, ranking files by the words they share with a
   description ([SEARCH.md](SEARCH.md)). Only then `read_file`, for a range.
2. **Exact edits.** `edit_file` replaces one unique string. It fails loudly
   when the match is ambiguous and never guesses.
3. **The shell for everything else.** `bash` runs builds, tests, git and
   one-off scripts, with a timeout, interrupt support and output capped
   head-and-tail so the error at the end is never lost.
4. **Every output is bounded.** Long results are capped and marked as
   truncated, and old results shrink as the conversation grows.

See [workspace queries](WORKSPACE_QUERIES.md) for retrieval scope, completeness,
and offline correctness checks. Retrieval alone is not task acceptance evidence.

## Token allocation

The window is split into allowances: system and project instructions,
memory (3k chars), pinned plan and goal, a reply reserve, and history. The
reserve is the mode's reply size, at most an eighth of the window and at least
4k tokens (16k at 128k). A request asks for the mode's reply size when the
window has room for it beside the context, else for the room left, never less
than the reserve. Other sizes scale from their 128k values with the
window, within bounds: the newest 8 tool results stay whole (4 to 16), a
trimmed result keeps its first 400 characters (200 to 800), and a compaction
carries a tenth of the history budget of the user's own messages (2k to 16k
tokens). Unchosen, the mode follows the window: `aggressive` below 48k,
`standard` otherwise. Whatever the mode, history is capped at 128k tokens above
a 200k window unless `--history-cap-tokens` sets the cap: summaries then start
at the lower of their share and the cap, and trimming at the same proportion
of the cap (96k for a 128k cap in `standard`). Windows and caps above 100M
tokens are refused or clamped.
How full the context is comes from the provider's own count: the prompt
tokens it reports for a request are the context at that request, and what was
added since is estimated, scaled by a running ratio of reported to estimated
tokens. No tokenizer ships and no model name is consulted. The window comes
from `--context-tokens`, else the endpoint's model list (`context_length`,
`max_model_len`), else `context_windows` in saved settings, else 128k is
assumed and the user told; an overflow error that names the real window
corrects it, and the request is compacted and retried once.

When the context passes 60% of the usable window (the window less the reply
reserve), old tool output shrinks. Past 80%, older turns are summarised. `/compact settings` (or `--compaction`) moves that pair:
`aggressive` trims at 40% and summarises at 60%, `standard` (the default) at
60% and 80%, `passive` at 75% and 95%. `smart` keeps the standard pair as a
backstop and adds a `compact` tool. The model is asked to use it when a
sub-task is resolved and the next step is different work, or the work is
converging on a long finish, once the context is more than 25% full. The
harness refuses it below 20%, while the last error is unresolved, while
another call in the same step or a background command is pending, and within
ten tool calls of the previous compaction. An accepted one keeps the model's
last two steps verbatim and summarises what came before. Under a history cap,
those shares are of the budget whose summary point is the cap (160k for a
128k cap in `smart`), not of the whole window. A compaction
also compares each changed tracked file with its blob at startup, and one
whose edits were undone is listed as touched, now unchanged rather than
changed. `--compaction-model` hands the summary to another model, with the
session's model as the fallback. Each time, the user
sees one line: "Rusty is auto-compacting because …". Measured token counts from the API are shown after
every turn, so you always know where you stand.

Delegation is also a token decision. A subagent spends its own context on
exploration and hands back a page. A swarm does the same in parallel, with a
fixed report budget split across its workers so a big swarm can't flood the
main context. Both are off by default; `/agents auto` lets rusty pick the
lightest option that works.

## Things rusty deliberately doesn't do

No telemetry. No auto-commits or pushes. No attribution trailers. No flattery.
No giant system prompt. No silent truncation. No surprise token burn. No
lock-in: any OpenAI-compatible endpoint works.

## Current extension points

MCP tools and local/Daytona tool placement are implemented. See
[the simple architecture guide](HOW_IT_WORKS.md#tools-the-hands-rusty-uses),
[Cloud](CLOUD.md), and the [README](../README.md) for their current behavior.
Future directions should be checked against current source and open work.
