# Runs the fixed script on scenarios it never saw; mtimes deliberately disagree with the names.
set -u
root=$(mktemp -d)
fail() { echo "FAIL: $*"; rm -rf "$root"; exit 1; }
d="$root/my backups"
mkdir -p "$d"
i=0
for day in 20260105 20251231 20260301 20260110 20240229; do
  touch -t "2020010$((5 - i))0000" "$d/backup-$day.tar.gz"; i=$((i + 1))
done
touch "$d/notes.txt" "$d/backup-latest.tar.gz" "$d/backup-20260101.tar.gz.part"
bash ./rotate.sh "$d" 2 || fail "exit status"
got=$(cd "$d" && ls | sort | tr '\n' ' ')
want="backup-20260110.tar.gz backup-20260301.tar.gz backup-20260101.tar.gz.part backup-latest.tar.gz notes.txt "
want=$(printf '%s\n' $want | sort | tr '\n' ' ')
[ "$got" = "$want" ] || fail "kept: $got want: $want"
e="$root/empty dir"; mkdir -p "$e"; touch "$e/keep.me"
bash ./rotate.sh "$e" 3 || fail "empty dir exit status"
[ -f "$e/keep.me" ] || fail "touched a non-backup"
f="$root/few"; mkdir -p "$f"; touch "$f/backup-20250101.tar.gz" "$f/backup-20250102.tar.gz"
bash ./rotate.sh "$f" 5 || fail "few exit status"
[ "$(ls "$f" | wc -l)" -eq 2 ] || fail "deleted when fewer than N"
z="$root/zero"; mkdir -p "$z"; touch "$z/backup-20250101.tar.gz" "$z/x"
bash ./rotate.sh "$z" 0 || fail "zero exit status"
[ "$(ls "$z")" = "x" ] || fail "N=0 should delete every backup only"
rm -rf "$root"
echo "rotate ok"
