# Product definition

## Product thesis

Git’s data model is durable, but local recovery is not presented as one coherent user operation. A destructive command can affect refs, HEAD, the index, tracked files, and untracked files at once. Recovery then depends on knowing which Git mechanism retained which part.

git2’s thesis is that a Git-compatible client can make this simpler by recording complete supported local views and operation transitions before it mutates anything.

## Intended user

The first user is a technically curious developer who:

- already uses Git locally;
- wants to understand Git internals;
- experiments in disposable repositories;
- values a visible, auditable operation history;
- accepts a narrow supported-repository profile in exchange for explicit safety behavior.

The first release is not intended for production teams, monorepos, regulated environments, or users who need every Git feature.

## Jobs to be done

The eventual MVP should let a user:

1. Adopt an existing ordinary Git repository without changing its public history.
2. Use familiar staging, commit, log, and reset commands.
3. See changes made by ordinary Git represented in one local operation history.
4. Preview what a restore or undo would affect.
5. Undo a supported destructive local operation without deleting the ability to redo it.
6. Continue using ordinary Git and push only standard refs and objects.

## Product promise for experimental v0.1

The v0.1 claim is intentionally narrow:

> In a supported Linux repository, git2 records a durable complete pre-operation view for each supported git2 mutation and can restore that view non-destructively, including HEAD, refs, index, tracked worktree content, and non-ignored untracked content.

The release must not make this claim until interruption testing and a full undo demonstration pass. The current alpha only captures and inspects views.

## Scope of a complete view

Included:

- raw HEAD;
- local loose refs and packed refs;
- raw Git index;
- present tracked files;
- present non-ignored untracked files;
- regular/executable distinction;
- symbolic-link targets;
- byte paths and file contents on Unix.

Excluded from v0.1:

- ignored files;
- Git config, hooks, credentials, and remotes;
- reflogs and unreachable Git objects as separate recovery data;
- submodule working trees;
- linked worktrees;
- sparse or partial working copies;
- ownership, ACLs, extended attributes, and filesystem timestamps;
- repositories using SHA-256 object IDs.

## Command scope

The v0.1 user-facing command set is planned as:

```text
git2 adopt
git2 checkpoint
git2 status
git2 oplog
git2 restore
git2 recover
git2 reset
git2 undo
git2 add
git2 commit
git2 log
```

Only `adopt`, `checkpoint`, `status`, and `oplog` exist today. Debug object commands are development aids and are not part of the product promise.

## Non-goals

For v0.1, git2 will not:

- replace Git’s repository or object format;
- invent a new branch model;
- provide hosting, pull requests, or authentication;
- implement merge, rebase, cherry-pick, stash, or bisect;
- implement clone, fetch, pull, or push;
- support every filesystem or repository extension;
- silently degrade recovery coverage;
- silently prune recovery history.

## Success criteria

### Correctness

- Every Git object written by git2 is accepted by `git fsck` and ordinary Git commands.
- Status and staging results match Git for the supported matrix.
- A round trip of capture → mutation → restore reproduces exact supported state.
- Non-UTF-8 paths and content round-trip on Unix.

### Safety

- Unsupported repository states are rejected before mutation.
- Every mutation has a durable pre-operation view and pending transaction.
- Forced termination at each write boundary is recoverable.
- Undo creates a new operation and never deletes earlier history.
- Recovery data cannot be pushed through ordinary refs, including `git push --mirror`.

### Usability

- Errors explain what is unsupported and how to proceed safely.
- `restore --dry-run` shows the affected refs, index, and worktree paths.
- `oplog` makes external-Git imports and git2 operations distinguishable.
- Documentation includes a reproducible disposable-repository demo.

### Maintainability

- Formatting, clippy, tests, and docs pass with warnings denied.
- Each mutation has differential and failure-injection coverage.
- On-disk format changes are documented and versioned.

## Release posture

Version `0.1.0-alpha.1` describes an in-development package, not a supported release. Until the roadmap’s v0.1 gate passes, every public document and binary description must continue to call the project experimental and unsafe for important repositories.
