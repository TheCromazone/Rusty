# Only the second session's work counts: the reset removed the first one's.
# The suite records a run only when the current seed (rotated, in the
# rewritten ONBOARDING.md) was set; the remembered one no longer works.
# The marker holds a value derived from the seed, so editing the guard or
# writing the file by hand does not produce it.
python3 -c "
from stats import median
assert median([3, 1, 2]) == 2 and median([4, 1, 3, 2]) == 2.5
try:
    median([]); raise SystemExit('no ValueError')
except ValueError:
    pass
" && grep -rqs "median" tests/ && [ "$(cat .suite_ran 2>/dev/null)" = 61df0cabcb1c63e163b71f08650a4decd89537aed2c88f3b0b3d81d36f7b8bf0 ]
