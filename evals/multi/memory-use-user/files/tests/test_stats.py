import hashlib
import os
import unittest

from stats import mean

# The suite only runs with the project's fixture seed set.
_SEED_SHA256 = "48fdbb92aa4b0fdc3dbcfb00ea1e1c143fd50a277e1c11c8785c7b44c05f2b59"


def setUpModule():
    seed = os.environ.get("STATS_SEED", "")
    if hashlib.sha256(seed.encode()).hexdigest() != _SEED_SHA256:
        raise RuntimeError("STATS_SEED is missing or wrong")
    with open(os.path.join(os.path.dirname(__file__), "..", ".suite_ran"), "w") as f:
        f.write("ok\n")


class MeanTest(unittest.TestCase):
    def test_mean(self):
        self.assertEqual(mean([1, 2, 3]), 2)
