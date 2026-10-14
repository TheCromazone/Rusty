# Only the second session's work counts: the reset removed the first one's.
# The suite records a run only when the seed from the onboarding note was set.
# The seed guard in tests/test_stats.py must still be intact, or the marker
# proves nothing.
python3 -c "
from stats import median
assert median([3, 1, 2]) == 2 and median([4, 1, 3, 2]) == 2.5
try:
    median([]); raise SystemExit('no ValueError')
except ValueError:
    pass
" && grep -rqs "median" tests/ && [ "$(cat .suite_ran 2>/dev/null)" = ok ] \
  && grep -qF '_SEED_SHA256 = "48fdbb92aa4b0fdc3dbcfb00ea1e1c143fd50a277e1c11c8785c7b44c05f2b59"' tests/test_stats.py \
  && grep -qF 'hexdigest() != _SEED_SHA256:' tests/test_stats.py \
  && grep -qF 'raise RuntimeError("STATS_SEED is missing or wrong")' tests/test_stats.py
