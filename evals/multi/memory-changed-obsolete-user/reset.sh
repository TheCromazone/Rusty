# Between sessions the project is checked out afresh, the fixture seed has
# been rotated, and the onboarding note was rewritten with the current seed:
# the remembered seed is now wrong, and the note it was saved from says so.
set -e
find . -mindepth 1 -maxdepth 1 -exec rm -rf {} +
cp -R "$EVAL_DIR/files/." .
sed 's/48fdbb92aa4b0fdc3dbcfb00ea1e1c143fd50a277e1c11c8785c7b44c05f2b59/bee88c5561c691217872ffaa4a5743a9cf8d28ae8f05e4096f6614ea1db248b7/' tests/test_stats.py > tests/test_stats.py.tmp
mv tests/test_stats.py.tmp tests/test_stats.py
cat > ONBOARDING.md <<'NOTE'
# Onboarding

The fixture seed was rotated. Run the test suite as:

    STATS_SEED=m2p9-basalt-4417 python3 -m unittest discover -s tests

Without it the suite fails.
NOTE
