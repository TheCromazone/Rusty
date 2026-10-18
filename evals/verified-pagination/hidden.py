"""Independent development grader; never copied into files/."""
import copy
import os
import sys

sys.path.insert(0, os.getcwd())
from pagination import collect


def check_pages(pages, start, expected, cycle=False):
    original = copy.deepcopy(pages)
    called = []

    def fetch(cursor):
        assert cursor not in called, "a repeated cursor was fetched"
        called.append(cursor)
        assert len(called) <= len(pages), "unbounded pagination"
        return pages[cursor]

    if cycle:
        try:
            collect(fetch, start)
        except ValueError:
            pass
        else:
            raise AssertionError("cycle did not raise ValueError")
    else:
        assert collect(fetch, start) == expected
    assert pages == original, "input pages changed"


check_pages({None: {"items": [], "next": None}}, None, [])
check_pages({None: {"items": [], "next": "a"}, "a": {"items": [], "next": "b"},
             "b": {"items": [4, 4, 2], "next": None}}, None, [4, 4, 2])
check_pages({"s": {"items": [0], "next": "a"}, "a": {"items": [1], "next": "s"}}, "s", None, True)
check_pages({"s": {"items": [], "next": "s"}}, "s", None, True)
check_pages({None: {"items": [7], "next": "a"}, "a": {"items": [], "next": None}}, None, [7])
error = RuntimeError("fetch failed")


def broken_fetch(cursor):
    raise error


try:
    collect(broken_fetch)
except RuntimeError as observed:
    assert observed is error
else:
    raise AssertionError("fetch error was swallowed")
print("independent checks passed")
