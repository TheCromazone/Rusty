# Only the session's own run of the suite counts. The suite records a run
# only when the seed, which only memory holds, was set.
python3 -c "
from stats import median
assert median([3, 1, 2]) == 2 and median([4, 1, 3, 2]) == 2.5
try:
    median([]); raise SystemExit('no ValueError')
except ValueError:
    pass
" && grep -rqs "median" tests/ && [ "$(cat .suite_ran 2>/dev/null)" = ok ]
