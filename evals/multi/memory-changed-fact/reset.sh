# Between sessions the project is checked out afresh and the onboarding note
# was rewritten without the run command. The seed itself did not change: only
# what the agent remembered says how to run the suite.
set -e
find . -mindepth 1 -maxdepth 1 -exec rm -rf {} +
cp -R "$EVAL_DIR/files/." .
cat > ONBOARDING.md <<'NOTE'
# Onboarding

Welcome. Ask in the team channel for access to the shared drive, and read
stats.py before your first change. Setup is covered in your first session.
NOTE
