# Only the second session's work counts: the reset removed the first one's.
# The suite records a run only when the seed from the onboarding note was set.
# The marker holds a value derived from the seed, so editing the guard or
# writing the file by hand does not produce it.
python3 -c "
from stats import median
assert median([3, 1, 2]) == 2 and median([4, 1, 3, 2]) == 2.5
try:
    median([]); raise SystemExit('no ValueError')
except ValueError:
    pass
" && grep -rqs "median" tests/ && [ "$(cat .suite_ran 2>/dev/null)" = f6ab05b52ad7080629d507794370bc33ef6f73b890e0d20a37c2c92daf3b6e63 ]
