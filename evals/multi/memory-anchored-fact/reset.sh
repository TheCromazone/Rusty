# Between sessions the project is checked out afresh, and the onboarding
# note is gone: only what the agent remembered says how to run the suite.
set -e
find . -mindepth 1 -maxdepth 1 -exec rm -rf {} +
cp -R "$EVAL_DIR/files/." .
rm ONBOARDING.md
