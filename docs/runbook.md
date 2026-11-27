# Operations, Limitations, and Recovery Runbook

> [!WARNING]
> `git2` is experimental v0.1 software and is **not safe for important repositories**. Always evaluate in disposable clones and keep an independent backup before testing recovery flows.

## 1. What `git2` protects (and what it refuses)

`git2` only manages local, single-worktree, non-bare SHA-1 Git repositories on Linux whose Git metadata lives directly in the worktree's own `.git/` directory.

Before any mutation, `git2` validates supported repository invariants and **refuses before touching Git state or creating pending records** when it sees:

- linked worktrees, separate `GIT_DIR` / `core.worktree`, bare repositories, shallow clones, partial/promisor clones, reftable refs, or alternate object stores;
- SHA-256 object format;
- submodules / gitlinks in index or retained trees, split indexes, sparse indexes (`SKIP_WORKTREE` / `core.sparseCheckout`), resolve-undo (`REUC`) index extensions, unresolved merge conflicts, or in-progress `MERGE_HEAD` / `rebase-*` / `CHERRY_PICK_HEAD` / `REVERT_HEAD` / `BISECT_LOG` / `sequencer`;
- active external `filter=` clean/smudge/process drivers (`git2 add`, `git2 reset --hard`) or non-UTF-8 `working-tree-encoding`;
- active executable commit hooks (`pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`), `commit.gpgSign = true`, non-UTF-8 `i18n.commitEncoding`, or `INTENT_TO_ADD` (`git add -N`) index entries during `git2 commit`;
- pre-existing foreign `.git/*.lock` files (`index.lock`, `HEAD.lock`, `packed-refs.lock`, or target ref `.lock`);
- legacy format `1` or format `2` `.git/git2/` recovery stores (which are left untouched on disk).

Ignored files, Git config files, reflogs, file ownership/ACLs/timestamps, and empty directories are outside the v0.1 recovery guarantee.

## 2. Recovering from an interrupted `git2` command

Every mutating `git2` command (`restore`, `reset`, `undo`, `add`, `commit`) writes an immutable plan and pending state into `.git/git2/pending/` **before** touching Git's index, refs, `HEAD`, or working tree.

If a process crashes or is killed mid-command, other `git2` commands stop and instruct you to inspect `.git/git2/pending/`:

### Step 1: Inspect pending state (read-only)

```sh
git2 recover --status
```

This prints the transaction ID, phase (`prepared`, `applying`, `finalizing`, `committed`), command, `before` and `target` view IDs, action progress, and deterministic operation ID without altering anything.

### Step 2: Reconcile or roll back

- **To complete and clean up cleanly** (the normal choice):
  ```sh
  git2 recover
  ```
  - If the transaction died in `prepared` (before any Git mutation started), `git2 recover` removes the pending state and leaves the repository untouched at `before_view`.
  - If it died in `applying` or `finalizing`, `git2 recover` verifies locks/live state and finishes the remaining actions, selects the operation ID in `.git/git2/HEAD`, and cleans `pending/`.
  - If it died in `committed` (after `.git/git2/HEAD` moved), `git2 recover` simply finishes cleaning `pending/` without duplicating history.
- **To undo an unfinished transaction back to `before_view`** (only allowed before `.git/git2/HEAD` moved, i.e., `prepared`, `applying`, or `finalizing`):
  ```sh
  git2 recover --rollback
  ```
- **To verify ordinary Git integrity afterwards**:
  ```sh
  git fsck --full --strict
  ```

### Step 3: Resolve foreign locks or manual divergence

If ordinary `git` left a stale `.git/index.lock` or ref `.lock`, or edited files concurrently while `pending/` existed:

1. Confirm no other `git` or `git2` process is running.
2. Remove stale foreign lock files or decide whether Git's state should win.
3. If `git2 recover` refuses because live files/refs diverged during the crash, save an external copy of your working tree before reconciling manually.

## 3. Restoring an earlier state manually

1. List immutable operations newest-first:
   ```sh
   git2 oplog
   ```
   Each line has `<operation-id> <unix-seconds> <kind> <before-view-id> -> <after-view-id>`.
2. Preview changes without modifying anything:
   ```sh
   git2 restore --view <64-hex-view-id> --dry-run
   ```
3. Apply the restore:
   ```sh
   git2 restore --view <64-hex-view-id>
   ```
   Or undo/redo the latest eligible `git2` mutation directly:
   ```sh
   git2 undo --dry-run
   git2 undo
   ```

## 4. Inspecting and managing `.git/git2/` disk usage

`git2` deduplicates unchanged content via SHA-256 zlib-compressed CAS files under `.git/git2/cas/`, but **never prunes historical views or objects silently**.

- Inspect sidecar disk usage:
  ```sh
  du -sh .git/git2/ .git/git2/cas/ .git/git2/views/ .git/git2/operations/
  ```
- Retire `git2` cleanly from a repository (first archive `.git/git2/` if you might still want past local views):
  ```sh
  tar -czf git2-sidecar-backup.tar.gz -C .git git2
  rm -rf .git/git2
  ```
  Removing `.git/git2/` does not affect ordinary Git commits, branches, objects, or remotes.
