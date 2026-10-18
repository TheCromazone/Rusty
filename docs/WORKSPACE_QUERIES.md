# Workspace queries

The query layer runs inside the existing process and reads the workspace on
request. It has no daemon, startup walk, dependencies, filesystem catalog,
content cache, or result cache. Local tools use it directly; the existing tool
bridge executes the same tools beside the remote files. Client files never
answer a remote query.

Searches and partial or truncated enumerations report two independent facts.
Small, complete listings, globs and outlines retain their plain result body:

- `scan_complete`: whether the requested scope was examined without known
  errors or enumeration/read limits. This does not certify an atomic snapshot.
- `output_truncated`: whether results were omitted or an engine limit may have omitted them from the display.
  An entirely scanned workspace can have a truncated display. Unread files
  make an outline incomplete even if its displayed text fits.

`freshness=live_observation cache=none retrieval_only=true` describes retrieval,
not a verification run. A query result is not fresh task acceptance evidence.
Concurrent edits can occur between files; line numbers describe the contents
observed by that scan. No snapshot identity or persisted freshness is claimed.

## Matching and visibility

Search invokes ripgrep on every request with smart case, line numbers, and
optional glob/context arguments (context is clamped to ten lines). It retains
ripgrep's default hidden, binary, and ignore rules, including explicit glob
overrides. Descendant symlinks are not followed. Ambient config is disabled.

Ripgrep keeps its parallel traversal: no `--sort path` is requested. Only the
retained prefix is sorted by path and numeric line number. When the byte cap is
reached, parallel arrival order can change which records fit; the truncated
subset is not promised to be repeatable. Context separators are omitted, while
matching and context line delimiters are preserved. NUL-delimited input paths
keep colons/newlines in filenames from corrupting per-file limit accounting.

Engine bounds are restored: `--max-count 30 --max-columns 300`. Reaching thirty
matching lines in any observed file conservatively sets `match_limit_possible`,
`scan_complete=false`, and `output_truncated=true`, even if a file happens to
contain exactly thirty matches. Column omission markers set
`column_limit_possible` and `output_truncated=true`; column clipping alone does
not make the scan incomplete. A literal omission marker can conservatively
trigger this flag too. An independent streaming audit examines every drained
record, including records beyond the retained display prefix. Its pending frame
and retained per-file path keys each have a 28,000-byte budget, with at most
20,000 path entries. Malformed frames or exhausted audit budgets set
`audit_complete=false` and prevent a complete-scan claim, without inventing an
observed match cap. Display clipping alone leaves scan completeness unchanged.
Later well-framed records survive a malformed numeric line. Empty
complete results still require an error-free, uncapped no-match observation.

The first 28,000 raw output bytes are retained while both pipes are drained;
partial records are omitted from presentation. All subprocess diagnostics survive partial
success, up to a separate 2,000-byte prefix with an omission marker. Lossy UTF-8
conversion can temporarily expand invalid raw bytes up to threefold. The retained
rendered prefixes are capped again at the same byte limits on a UTF-8 boundary,
excess String capacity is released, and any additional omission sets
`output_truncated` or the diagnostic marker. This bounds retained query output,
not process RSS or the allocator's freed-page retention. The entire rendered
response also has a 30,000-byte cap, preserving its completeness header and
bounded diagnostics and setting `output_truncated=true` when clipped. Outlines
retain at most 160 bytes from each definition signature.

Only failure to find the ripgrep executable selects grep. The legacy grep
fallback uses case-insensitive extended regex and build-directory exclusions;
it cannot promise ripgrep ignore, glob, or regex parity. Its results always say
`scan_complete=false`, including when grep exits successfully or finds nothing.
An invalid regex, unreadable path, or failed ripgrep process never becomes a
complete empty result.

On Unix, query subprocesses have owned process groups and a 30-second deadline.
The coordinator drains both pipes with nonblocking reads and bounded prefixes;
it starts no reader threads. A per-iteration read budget prevents chatty stdout
from starving stderr or deadline checks. Normal exit reaps the leaf search
process and disarms every PID/group signal path. Timeout, caller cancellation,
and I/O/setup failures signal only a verified owned, unreaped group and then
reap through the same owner. An exited child gets at most 100 milliseconds
to close inherited pipes before the coordinator closes its descriptors. Escaped
process groups and holders surviving a normal leaf exit are outside containment,
but cannot strand a query reader thread. Nondefault SIGCHLD reaping policies
are rejected before spawning. `termination=deadline`, `cancelled`, or `inherited_pipes` makes a
search incomplete. Only `termination=exited` can report complete. OS scheduling
and uninterruptible kernel operations prevent a hard real-time guarantee.
Non-Unix search fails before spawning until bounded pipe support is implemented.

Glob and outline retain their separate existing policy: include hidden files,
do not apply ignore files, and exclude `.git`, `target`, `node_modules`, `.venv`,
`venv`, `__pycache__`, `dist`, `build`, and `.next` directory names. Globs retain
the tool's `*`, `?`, `**` matcher. A basename-only pattern matches basenames;
a pattern with `/` matches relative paths. No approximate candidate filter is
used. Descendant directory symlinks are reported as skipped rather than
followed into cycles. Regular-file symlinks are read normally.

Locate lists the files a search would read with `rg --files` (ripgrep's
hidden and ignore rules, the same deadline and process ownership), keeping at
most 20,000 paths; a longer listing is reported incomplete. It then reads and
indexes every listed file for that call only and drops the index, so it has
no cache either. Files it cannot read make its scan incomplete; secrets, lock
files, binaries, minified text and files over 1 MB are left out by policy.
Without ripgrep it falls back to the glob/outline enumeration and reports
that as a skipped scope. [SEARCH.md](SEARCH.md) has its ranking and costs.

Enumeration is bounded to 20,000 visited entries and 128 directory levels.
The global budget is charged during directory enumeration, bounding retained
pending entries across nested traversal as well as returned paths.
Exhausted budgets identify the omitted directory/subtree. Read and metadata
errors identify skipped paths. At most eight diagnostic paths are displayed;
the skipped count includes omitted entries/subtrees, not an exact count of
unseen files. Glob displays up to 300 names. Outline reads up to 60 eligible
source files and stops reading at the display budget; unread files explicitly
make its scan incomplete. Listing retains its requested depth scope, 2,000
entry traversal budget and 500 displayed entries. Depth outside the requested
scope is identified separately from traversal failure.

## Correctness and measurement

Run the deterministic offline checks with ripgrep installed:

```sh
cargo test --lib workspace_query
cargo test --bin rusty tools::tests
cargo test --test workspace_query
cargo test --test workspace_query benchmark_query_reference_and_legacy -- --ignored --nocapture
cargo xtask qa
```

Below all limits, fixtures compare canonical match/context records with an
independent unbounded direct ripgrep invocation; engine traversal order is not
part of that comparison. At limits, fixtures check bounded output and explicit
incompleteness/omission metadata rather than unbounded-output equivalence. They exercise smart case, regex alternatives, glob/context
combinations, ignore/hidden rules, more than thirty matches in one file, long
lines, empty results followed by additions or edits, same-length overwrites,
renames, deletion, ignore-file edits, failed shell commands that write, and
separate Git worktrees. Error fixtures cover partial results with unreadable
files, missing roots, invalid regex, directory symlinks, dangling symlinks,
enumeration budgets, and grep fallback. Tool checks exercise glob mutation and
outline/listing completeness at their integration point. Cleanup checks exercise
a chatty deadline, run-scoped cancellation, repeated child reaping, ordinary descendant pipe holders,
and escaped pipe holders without background reader threads. A real-CLI offline
fixture sends SIGINT during a scripted search, checks the query leader is reaped,
and verifies the exported result says cancelled/incomplete with no verification
receipt. Raw terminal key-byte handling remains owned by the UI coordinator;
this fixture proves the signal-handler and run-cancellation path.

The opt-in benchmark alternates old and current execution order over the same
400-file fixture. Every pair must return the same match set. It reports one
first invocation and fifteen warm repetitions per variant. First invocation is
not a claim of a cold OS page cache. This is query plumbing measurement on one
machine, not a model solve-rate, cost, or acceptance experiment.

Caching is deferred. Repeated synthetic queries demonstrate possible reuse,
not its frequency in real investigations. A cache needs measured session-level
reuse and mutation costs before its benefit can justify freshness complexity.
In particular, caching only matching files would miss a newly matching file
after an empty result. Repeating a cached retrieval must never create fresh
acceptance evidence. Any future cache must preserve these fixtures and add
explicit hit/miss, validation, eviction and fallback receipts.

### Historical same-machine receipt (before subprocess cleanup and bounded parallel presentation)

On October 17, 2026, the exact query source passed the standalone module and
reference fixtures on a 10-core, 16 GiB macOS machine while other builds were
running (load average roughly 58–86). The 400-file sparse-match benchmark
checked equal match sets in all sixteen alternating pairs. Warm medians over
fifteen repetitions were 27,520 µs for the legacy invocation and 27,983 µs for
the current query layer. The first invocations were 216,095 µs and 58,796 µs,
respectively; neither is an OS-cache-cold measurement. The legacy timing omits
its one-time executable availability probe and compares subprocess retrieval,
not the older lossy tool presentation. These noisy plumbing timings establish
no meaningful speedup and provide no real-session cache reuse rate. They
support retaining the direct-scan implementation until session traces justify
an additional optimization.
