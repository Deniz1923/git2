#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GIT2_BIN="${GIT2_BIN:-}"

run_git2() {
  if [[ -n "$GIT2_BIN" ]]; then
    "$GIT2_BIN" "$@"
  else
    cargo run --quiet --manifest-path "$ROOT_DIR/Cargo.toml" -p git2-cli -- "$@"
  fi
}

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

REPO="$TMP_DIR/demo-repo"
mkdir -p "$REPO"
cd "$REPO"

export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME="git2 Demo"
export GIT_AUTHOR_EMAIL="demo@example.invalid"
export GIT_COMMITTER_NAME="git2 Demo"
export GIT_COMMITTER_EMAIL="demo@example.invalid"

echo "==> Creating disposable repository at $REPO"
git init --quiet -b main
printf 'hello from commit 1\n' > app.txt
git add app.txt
git commit --quiet -m "initial baseline"

echo "==> Adopting repository into .git/git2/"
adopt_out="$(run_git2 adopt)"
echo "$adopt_out"
baseline_view="${adopt_out##* as view }"

echo "==> Making tracked, new, and untracked changes, staging with git2 add, and committing"
printf 'hello from commit 2\n' > app.txt
printf 'feature notes\n' > notes.txt
run_git2 add app.txt notes.txt
run_git2 commit -m "second commit via git2"

printf 'draft untracked scratchpad\n' > scratch.txt
run_git2 status

echo "==> Showing bounded git2 log"
run_git2 log -n 5

echo "==> Running destructive git2 reset --hard HEAD~1"
run_git2 reset --hard HEAD~1
echo "app.txt after reset --hard:"
cat app.txt
echo "scratch.txt exists? $(test -e scratch.txt && echo yes || echo no)"

echo "==> Recovering non-destructively with git2 undo"
run_git2 undo
echo "app.txt after undo:"
cat app.txt
echo "scratch.txt exists after undo? $(test -e scratch.txt && echo yes || echo no)"

echo "==> Previewing restore to baseline view $baseline_view (--dry-run)"
run_git2 restore --view "$baseline_view" --dry-run

echo "==> Checking recovery log and pending status"
run_git2 oplog
run_git2 recover --status

echo "==> Verifying Git repository integrity with git fsck --full --strict"
git fsck --full --strict --no-progress
du -sh .git/git2
echo "Demo completed cleanly."
