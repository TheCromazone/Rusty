# Current CLI recordings

These clips are actual pseudo-terminal sessions of Rusty, using the live
`nvidia/nemotron-3-super-120b-a12b` model through NVIDIA's compatible endpoint.
`permissions` only runs local slash commands and makes no inference request.
The checked-in `manifest.json` records the source revision, per-scene outcomes,
and provider request accounting. These are demonstrations, not benchmarks.

## Watch

| Recording | What to watch for |
|---|---|
| [Standard](mode-standard.gif) · [cast](mode-standard.cast) | Failing tests, source edits, passing tests |
| [Careful](mode-careful.gif) · [cast](mode-careful.cast) | The same repair plus a separate read-only checker |
| [Vibe](mode-vibe.gif) · [cast](mode-vibe.cast) | A quick repair with an executed check |
| [Auto](mode-auto.gif) · [cast](mode-auto.cast) | Profile selection for a fix, prototype, and production-risk review |
| [Views](views.gif) · [cast](views.cast) | Default, verbose, and ADHD views doing source reads and test runs |
| [Permissions](permissions.gif) · [cast](permissions.cast) | Classifier previews for read-only, ask, auto, and yolo |
| [Verified goal](verified-goal.gif) · [cast](verified-goal.cast) | Fixed acceptance command followed by goal completion |
| [Swarm](swarm.gif) · [cast](swarm.cast) | Read-only module workers and the lead's report |
| [Delegation](delegation.gif) · [cast](delegation.cast) | A subagent, automatic selection, then delegation off |

Each scene starts from a fresh copy of `demo/shop`. The fixture intentionally
has two failing discount tests. The execution-mode repair scenes and verified
goal are expected to make the complete test suite pass. View and audit scenes leave
the fixture unchanged and run focused checks where requested.

The GIFs retain terminal output and order. Playback is 1.5×, with idle gaps
limited to 1.5 seconds, a 12 FPS cap, and the initial 0.3 seconds of terminal
startup omitted. Raw `.cast` files retain the original timing. No model
output, tool output, or test result is written into the recording by the scene.

## Record a scene

Build from the repository root:

```sh
cargo build --locked --bins
cargo build --locked -p xtask
```

Export your provider key in your shell. Use an isolated home so personal
sessions, memory, and preferences do not appear in public recordings. Then,
from the repository root:

```sh
repo="$PWD"
scratch="$(mktemp -d)"
cp -R demo/shop "$scratch/shop"
cd "$scratch/shop"
git init -q
git add .
git -c user.name=Demo -c user.email=demo@example.invalid commit -qm 'Demo fixture'
RUSTY_BIN="$repo/target/debug/rusty" \
RUSTY_HOME="$scratch/home" RUSTY_NO_DOTENV=1 \
"$repo/target/debug/xtask" record mode-standard "$scratch/mode-standard.cast" -- \
  --model nvidia/nemotron-3-super-120b-a12b \
  --mode standard --agents off --memory off --context-tokens 131072 \
  --max-requests 32 --max-budget-tokens 500000 --budget-secs 480
python3 -m unittest -q
agg --theme github-dark --font-size 16 --speed 1.5 --idle-time-limit 1.5 \
  --fps-cap 12 --select "0.3..100%" \
  "$scratch/mode-standard.cast" "$scratch/mode-standard.gif"
```

Replace `mode-standard` with a scene in the table. For `verified-goal`, also
pass `--goal 'fix both discount bugs without changing the existing tests. run all tests.'`
and `--verify 'python3 -m unittest -q'` after the recorder's `--` separator.
Always start with another fresh fixture and home. `mode-auto` changes profiles inside the session;
`swarm` and `delegation` explicitly enable their workers. The token admission
limit is not a dollar billing cap. Actual results, timing, and request counts
vary with the model and provider.

Scene definitions live in [`xtask/src/record.rs`](../../xtask/src/record.rs).
A scene aborts if its expected terminal output does not arrive, rather than
publishing an incomplete recording as a success. Inspect the cast, final diff,
and tests before replacing a public GIF.
