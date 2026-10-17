# Notes for anyone changing rusty, human or tool

- Keep it small. Few abstractions, few dependencies, no frameworks. Add a crate
  only when the standard library really can't do the job.
- Commits follow Conventional Commits, with one logical change each and the
  body explaining why (see CONTRIBUTING.md). Enable the hook with
  `cargo xtask install-hooks`.
- Pull requests read like a person wrote them: plain language, no mention of
  AI tools or coding agents, no "generated with" lines, no co-author trailers
  for tools.
- `cargo xtask qa` must pass before you push. For behaviour that depends on the
  model, run `cargo xtask qa --live` or the relevant `cargo xtask eval <task>`.
- Never commit `.env` or print a key. Keys reach child processes and
  containers as environment variables, never as arguments.
- Terminal UI changes need a real capture (`cargo xtask tty-capture`, then
  `cargo xtask tty-inspect`) checked for alignment, clipping and both narrow
  and wide terminals.
- Eval checks stay hidden from the agent: never copy `check.sh` or `check.py`
  into the task's `files/`.

## Performance

- `cargo xtask bench` measures what a user waits on, offline and from fixed
  inputs: startup, `--sessions` over 500 and 5000 sessions, redaction,
  recall, a turn on a 300-message history, a 5 MB streamed answer and the
  binary sizes. It prints one table and writes JSON (`--json PATH`).
- Before and after: build main, keep its binary, then run
  `cargo xtask bench --binary OLD` and `cargo xtask bench` on your branch on
  the same machine. Compare cpu ms, RSS and allocation counts; wall time moves
  with whatever else the machine is doing. `--only GROUP` reruns one part.
- A perf PR carries both tables with a delta column, says what it traded,
  and changes no behaviour. A new crate for speed needs a win in that table
  the standard library can't match.
- Budgets main met at 07498ea; nothing may exceed them, and a PR that wins
  tightens them:
  - `--help`, `--version` and an empty `--sessions`: 10 ms cpu, 10 MB RSS.
  - `--sessions` list or search over 5000 sessions: 30 ms cpu, 15 MB RSS,
    and a catalog on disk under twice the transcripts it indexes.
  - A turn on a 300-message history: under 3 ms between requests.
  - Redaction: 30 MB/s or more. A 5 MB streamed answer: under 50 MB RSS.
  - Release `rusty`: under 8 MB.
- Rules for new code: `--help`, `--version`, `--capabilities` and
  `--sessions` spawn nothing, touch no network and walk no tree; a model step adds no copy or serialization of the whole history;
  text passes such as redaction are single-pass with no allocation per token;
  streaming work per delta is proportional to the delta, never to the answer
  so far.
