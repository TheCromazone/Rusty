# Workspace queries

The query layer runs inside the existing process and reads the workspace on
request. It has no daemon, startup walk, dependencies, filesystem catalog,
content cache, or result cache. Local tools use it directly; the existing tool
bridge executes the same tools beside the remote files. Client files never
answer a remote query.

Every search, glob, listing, and outline reports two independent facts:

- `scan_complete`: whether the requested scope was examined without known
  errors or enumeration/read limits. This does not certify an atomic snapshot.
- `output_truncated`: whether computed results were omitted from the display.
  An entirely scanned workspace can have a truncated display. Unread files
  make an outline incomplete even if its displayed text fits.

`freshness=live_observation cache=none retrieval_only=true` describes retrieval,
not a verification run. A query result is not fresh task acceptance evidence.
Concurrent edits can occur between files; line numbers describe the contents
observed by that scan. No snapshot identity or persisted freshness is claimed.

## Matching and visibility

Search invokes ripgrep on every request with smart case, stable path order,
line numbers, and the existing optional glob/context arguments (context is
clamped to ten lines). It retains ripgrep's default hidden, binary, and ignore
rules, including explicit glob overrides. Descendant symlinks are not followed
by search; explicit file paths retain ripgrep's usual handling. It disables ambient ripgrep config
so a user's config cannot silently change the tool's flags. Results now include
filenames even when the scope is a single file. There is no per-file match or
column limit; the first 28,000 output bytes are retained while both output and
diagnostics are drained to EOF. All subprocess diagnostics survive partial
success, up to a separate 2,000-byte prefix with an omission marker.

Only failure to find the ripgrep executable selects grep. The legacy grep
fallback uses case-insensitive extended regex and build-directory exclusions;
it cannot promise ripgrep ignore, glob, or regex parity. Its results always say
`scan_complete=false`, including when grep exits successfully or finds nothing.
An invalid regex, unreadable path, or failed ripgrep process never becomes a
complete empty result. This implementation does not add a query timeout.

Glob and outline retain their separate existing policy: include hidden files,
do not apply ignore files, and exclude `.git`, `target`, `node_modules`, `.venv`,
`venv`, `__pycache__`, `dist`, `build`, and `.next` directory names. Globs retain
the tool's `*`, `?`, `**` matcher. A basename-only pattern matches basenames;
a pattern with `/` matches relative paths. No approximate candidate filter is
used. Descendant directory symlinks are reported as skipped rather than
followed into cycles. Regular-file symlinks are read normally.

Enumeration is bounded to 20,000 visited entries and 128 directory levels.
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

The fixtures compare exact search output with an independent unbounded direct
ripgrep invocation. They exercise smart case, regex alternatives, glob/context
combinations, ignore/hidden rules, more than thirty matches in one file, long
lines, empty results followed by additions or edits, same-length overwrites,
renames, deletion, ignore-file edits, failed shell commands that write, and
separate Git worktrees. Error fixtures cover partial results with unreadable
files, missing roots, invalid regex, directory symlinks, dangling symlinks,
enumeration budgets, and grep fallback. Tool checks exercise glob mutation and
outline/listing completeness at their integration point.

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

### Initial same-machine receipt

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
