# The codename exists only in the agent's first reply: no file, no user
# message and no tool call holds it, so after a compaction only the summary
# can carry it. The verdict is whether CODENAME.txt matches that reply. The
# lines printed say whether a compaction fell in between and whether its
# summary kept the word, which a pass without a boundary can't show.
python3 - "$RUSTY_HOME/trajectory.json" <<'PY'
import json, re, sys
try:
    t = json.load(open(sys.argv[1]))
except OSError:
    sys.exit("no trajectory: the session never finished")
msgs = t["messages"]
def text(m):
    c = (m or {}).get("content")
    if isinstance(c, list):
        c = " ".join(p.get("text", "") for p in c if isinstance(p, dict))
    return c or ""
first_user = next((i for i, m in enumerate(msgs) if m["role"] == "user" and "codename" in text(m)), None)
if first_user is None:
    sys.exit("the first turn never ran")
reply = next((m for m in msgs[first_user + 1:] if m["role"] == "assistant" and text(m).strip()), None)
words = re.findall(r"[a-z]+", text(reply).lower()) if reply else []
if len(words) != 1:
    sys.exit(f"the first reply should be one word, got: {text(reply)[:200]!r}")
chosen = words[0]
try:
    written = open("CODENAME.txt").read().strip().lower()
except FileNotFoundError:
    written = None
reply_at = msgs.index(reply)
ask_at = max((i for i, m in enumerate(msgs) if m["role"] == "user" and "CODENAME.txt" in text(m)), default=len(msgs))
between = [c for c in t.get("compactions") or [] if reply_at < c["boundary"] <= ask_at]
kept = any(chosen in text(msgs[c["boundary"]]).lower() for c in between if c["boundary"] < len(msgs))
print(f"chosen={chosen} written={written} compactions_between={len(between)} summary_kept={kept}")
sys.exit(0 if written == chosen else 1)
PY
