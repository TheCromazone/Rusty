"""Shared grader for the delegate-* tasks.

The verdict is the answer in ANSWER.txt. The printed line measures the
delegation itself, from the run's trajectory: how many workers ran, how many
path:line references their reports gave, how many point at nothing in the
working copy, and whether rusty told the lead about them.

    python3 delegate-check.py NAME [NAME ...]   # accepted answers
"""
import json, os, re, sys

accepted = set(sys.argv[1:])
answer = open("ANSWER.txt").read().strip() if os.path.exists("ANSWER.txt") else ""
messages = json.load(open(os.path.join(os.environ["RUSTY_HOME"], "trajectory.json")))["messages"]

delegated = {c["id"] for m in messages if m["role"] == "assistant"
             for c in m.get("tool_calls") or [] if c["function"]["name"] in ("task", "swarm")}
reports = [m["content"] for m in messages if m["role"] == "tool" and m.get("tool_call_id") in delegated]

ref = re.compile(r"(?<![\w/.])((?:\./)?[\w./-]+\.\w+):(\d+)(?:-(\d+))?")
refs = bad = 0
for report in reports:
    body = "\n".join(l for l in report.splitlines() if not l.startswith("Reference check:"))
    for path, start, end in set(ref.findall(body)):
        if ".." in path or path.startswith("/"):
            continue
        refs += 1
        start, end = int(start), int(end or start)
        try:
            lines = open(path, "rb").read().count(b"\n")
        except OSError:
            bad += 1
            continue
        bad += start == 0 or start > end or end > lines + 1
flagged = sum("Reference check:" in r for r in reports)
chars = sum(len(r) for r in reports)
print(f"answer={answer!r} workers={len(reports)} refs={refs} bad_refs={bad} flagged={flagged} report_chars={chars}")
sys.exit(0 if answer in accepted else 1)
