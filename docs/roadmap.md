# Roadmap

Last reconciled with the implementation: 2026-08-05.

## How to read this roadmap

This is an execution plan, not a feature wishlist. A milestone is complete only when its behavior, safety properties, interoperability tests, documentation, and failure modes meet the listed exit criteria.

Status labels:

- **Done**: implemented and covered by the current quality gates.
- **In progress**: part of the milestone works, but the exit criteria do not all pass.
- **Planned**: designed at roadmap level but not implemented.
- **Deferred**: explicitly outside the v0.1 critical path.

Planning assumptions:

- one primary developer;
- approximately 5–8 focused hours per week;
- correctness and learning value take priority over calendar speed;
- Linux is the v0.1 platform;
- ordinary Git is the compatibility oracle;
- no mutating command ships before the pending-transaction foundation.

At that pace, the remaining v0.1 work is roughly 22–32 focused weeks, or about 5–8 months. This is an estimate, not a release date.

## Current summary

| Milestone | Status | Product evidence |
| --- | --- | --- |
| 0. Repository rescue | Done | Honest two-crate workspace, private artifacts removed, clean gates |
| 1. Git object interoperability | Done | Loose round trips, packed reads, byte-safe commit/tree tests |
| 2. Adoption and immutable history | Done | CAS, views, operations, lock, external import |
| 3. Status | Done | Differential two-column porcelain and `-z` output |
| 4. Mutation transaction foundation | Planned | Required before staging |
| 5. Staging | Planned | `git2 add` with Git-compatible index writes |
| 6. Commit and history | Planned | `commit` and `log` |
| 7. Reset and restore engine | Planned | General view restoration and reset |
| 8. Non-destructive undo proof | Planned | `undo`, redo path, interruption tests |
| 9. Experimental v0.1 release | Planned | Packaging, CI, demo, limitations, checksums |

## Milestone 0 — repository rescue

**Status: Done**

Delivered:

- removed private conversation transcripts and abandoned planning artifacts;
- removed speculative backend/workflow crates and placeholder commands;
- reduced the workspace to `git2-core` and `git2-cli`;
- pinned Rust 1.96.0 and workspace lint policy;
- replaced ignored `todo!` tests with working vertical-slice tests;
- added MIT licensing and truthful project documentation;
- established formatting, clippy, tests, and warning-denied docs as gates.

Exit criteria:

- no public command exists solely as a placeholder;
- no private transcript remains in the source inventory;
- the workspace builds and tests without warnings;
- README distinguishes current behavior from the intended product.

## Milestone 1 — Git object interoperability

**Status: Done**

Delivered:

- full SHA-1 object ID parsing and formatting;
- canonical Git object framing and hashing;
- zlib loose-object reading and atomic writing;
- content-hash verification on loose reads;
- packed-object fallback through `gix`;
- byte-safe signatures, commits, unknown/multiline headers, and messages;
- binary tree parsing with non-UTF-8 names and gitlink representation;
- debug hash/cat commands;
- differential tests against `git hash-object`, `git cat-file`, and packed objects after `git gc`.

Exit criteria met:

- Git and git2 agree on blob IDs;
- each implementation can read loose blobs written by the other;
- git2 reads a packed blob after loose-object removal by Git GC;
- object unit tests cover corruption-sensitive encodings.

## Milestone 2 — adoption and immutable local history

**Status: Done for capture; restoration intentionally excluded**

Delivered:

- discovery and validation of the supported repository profile;
- refusal of bare repositories, linked worktrees, sparse indexes, partial clones, submodules, conflicts, and known in-progress operations;
- eligible tracked and non-ignored untracked file enumeration;
- configurable 100 MiB default per-file fail-safe;
- SHA-256 CAS with deduplication and integrity verification;
- byte-safe view and operation encodings with bounded field/count decoding;
- raw HEAD, refs, packed refs, index, file content, mode, and symlink target capture;
- advisory writer locking;
- adoption baseline and atomic recovery HEAD;
- external-change import without duplicate unchanged operations;
- CLI operation log.

Exit criteria met:

- ignored files are demonstrably excluded;
- oversized files refuse adoption before sidecar creation;
- known in-progress Git state is refused;
- imported external changes form a correct parent/before/after chain;
- corrupt/truncated records and concurrent git2 lock acquisition are tested.

Known debt carried forward:

- capture consistency against concurrently running ordinary Git is not proven;
- encoders do not preflight decoder limits and record files are read whole before decoding;
- custom capture limits are not persisted for automatic imports;
- directory `fsync` handling needs a documented release policy;
- no streaming capture or garbage collection;
- no restore exists yet.

## Milestone 3 — status

**Status: Done for the initial supported matrix**

Delivered:

- HEAD → index and index → worktree comparison;
- deterministic byte-path ordering;
- two-column status model;
- added, modified, deleted, type-change, rename/copy, conflict, and untracked model codes;
- raw newline output and NUL-delimited `-z` output;
- differential fixture covering staged addition, staged-plus-unstaged modification, unstaged deletion, and untracked file;
- automatic external import on adopted repositories.

Follow-up coverage before v0.1:

- executable-bit changes;
- symlinks and file/type transitions;
- non-UTF-8 and newline paths through CLI `-z`;
- ignore hierarchy and global excludes;
- unborn HEAD and detached HEAD;
- rename/copy behavior and output contract.

Rename/copy detection is not enabled in the current command, and unresolved index conflicts are refused by repository validation. Those codes are representable but are not yet claimed as supported detected output.

## Milestone 4 — mutation transaction foundation

**Status: Planned — next milestone**

Estimated effort: 2–4 focused weeks.

Deliverables:

1. Define and document a versioned pending-transaction record.
2. Add `RecoveryStore` APIs to create, advance, read, and clear one pending transaction under the advisory lock.
3. Capture a pre-operation view and sync it before any mutation callback runs.
4. Add a transaction runner that finalizes an after-view and immutable operation only on success.
5. Reconcile pending state on startup.
6. Detect or refuse concurrent ordinary-Git changes across capture/mutation boundaries.
7. Add deterministic failpoints after every durable write and before/after each mutation boundary.
8. Build process-kill tests that reopen the repository and prove an actionable state remains.
9. Reject encoder inputs that exceed decoder limits and bound whole recovery-record reads.
10. Persist repository recovery configuration, including the chosen file-size limit.
11. Define and enforce sidecar permission expectations.

Exit criteria:

- a no-op synthetic mutation produces a valid before/after operation;
- killing the process at every failpoint never loses the pre-operation view;
- restart reconciliation is idempotent;
- lock contention fails immediately without partial mutation;
- pending corruption is refused with no Git-state write;
- every newly written view/operation is guaranteed readable under the same format limits;
- the format and recovery procedure are documented byte for byte.

Stop condition: do not begin user-facing index or ref mutation until this gate passes.

## Milestone 5 — Git-style staging

**Status: Planned**

Estimated effort: 3–5 focused weeks after Milestone 4.

Initial command:

```text
git2 add [--all] [--force] <pathspec>...
```

Deliverables:

- parse byte-safe repository-relative pathspecs on Unix;
- use `gix` index/filter/attribute plumbing;
- write blobs compatible with Git clean filters and line-ending rules;
- add new files and update modified files;
- stage deletions with `--all`;
- preserve executable and symlink modes;
- obey ignore rules and require `--force` for ignored additions;
- use Git index lockfile semantics;
- wrap every index mutation in the transaction runner;
- append `git2:add` operations;
- refresh status correctly after staging.

Differential matrix:

- new/modified/deleted files;
- executable-bit changes;
- symlinks;
- empty files and binary content;
- non-UTF-8 paths;
- nested `.gitignore` and forced ignored files;
- `.gitattributes`, CRLF, and clean filters that do not require interactive input;
- partial pathspecs and `--all`;
- unborn HEAD;
- lock contention and interrupted index write.

Exit criteria:

- Git and git2 produce equivalent index entries for the supported matrix;
- `git diff --cached` shows equivalent changes;
- `git fsck` succeeds;
- every successful add has a complete recoverable before/after operation;
- every failpoint leaves a valid index or an actionable pending transaction.

## Milestone 6 — commit and history

**Status: Planned**

Estimated effort: 4–6 focused weeks.

Commands:

```text
git2 commit -m <message>
git2 log [--max-count <n>]
```

Deliverables:

- build canonical trees recursively from stage-zero index entries;
- reject unmerged index stages;
- resolve author/committer identity and timezone with clear precedence;
- create root and single-parent commits;
- preserve byte-safe commit messages internally;
- update symbolic or detached HEAD with transactional ref compare-and-swap;
- write reflog entries compatible with Git, or explicitly document any initial limitation;
- wrap commit/ref updates in the mutation transaction;
- show first-parent/full-parent history without loading unbounded history;
- append `git2:commit` operations.

Differential matrix:

- root commit;
- normal parent commit;
- empty tree/files and nested directories;
- executable and symlink tree modes;
- detached HEAD;
- concurrent ref movement;
- packed parent/tree reads;
- author/committer environment and config cases.

Exit criteria:

- `git log`, `git cat-file`, and `git fsck` accept every git2 commit fixture;
- tree IDs match Git for equivalent index state;
- ref races fail without overwriting another writer;
- interrupted commit/ref updates reconcile through pending state;
- `git2 log` has bounded traversal and corruption errors.

## Milestone 7 — reset and general restore engine

**Status: Planned**

Estimated effort: 5–7 focused weeks.

Commands:

```text
git2 reset [--soft|--mixed|--hard] <commit>
git2 restore --view <recovery-id> --dry-run
git2 restore --view <recovery-id>
```

Deliverables:

- resolve full commit IDs and a deliberately small revision syntax;
- implement soft, mixed, and hard reset semantics for supported repositories;
- verify every target operation/view/CAS record before mutation;
- compute a restore plan for HEAD, refs, index, files, modes, symlinks, and removals;
- display a byte-safe dry-run plan;
- snapshot current state before restore;
- write files without following symlinks or escaping the worktree;
- preserve ignored files;
- use atomic temp/write/rename where possible;
- make multi-path restore restartable through pending phases;
- append `git2:reset` and `git2:restore` operations.

Exit criteria:

- soft/mixed/hard results match Git for the supported matrix;
- restoring a captured view reproduces its modeled bytes exactly;
- extra eligible files are removed while ignored files remain untouched;
- non-UTF-8 paths, symlinks, executable bits, and deleted tracked paths round-trip;
- restore interruption at every path boundary can be resumed or safely reconciled;
- traversal and symlink-attack tests cannot write outside the worktree.

## Milestone 8 — non-destructive undo proof

**Status: Planned**

Estimated effort: 3–5 focused weeks.

Commands:

```text
git2 undo [<operation-id>]
git2 undo --dry-run [<operation-id>]
```

Deliverables:

- default to undoing the selected operation’s transition;
- restore the chosen operation’s `before` view using the general restore engine;
- append a new `git2:undo` operation instead of moving/deleting old history;
- make redo possible by undoing the undo transition or through an explicit follow-up UI decision;
- show conflicts when current state diverges from the expected selected view;
- define policy for undoing `external-git` operations;
- make operation IDs safely abbreviatable only when unique;
- provide a complete recorded demonstration.

Core demonstration:

1. Adopt a disposable repository containing tracked, staged, modified, deleted, executable, symlink, and non-ignored untracked state.
2. Perform a destructive hard reset through git2.
3. Verify ordinary Git sees the reset state and `git fsck` passes.
4. Run `git2 undo`.
5. Verify HEAD, refs, raw index, all eligible bytes, modes, symlinks, and absences equal the pre-reset view.
6. Verify ignored files were untouched.
7. Verify the reset and undo operations both remain in `oplog`.
8. Repeat with forced termination at every transaction phase.

Exit criteria:

- the demonstration passes automatically and manually;
- undo never deletes an operation or immutable record;
- undo itself can be undone;
- divergence is detected rather than silently overwritten;
- documentation stops warning that snapshots are not restorable only after this gate passes.

## Milestone 9 — experimental v0.1 release

**Status: Planned**

Estimated effort: 3–5 focused weeks.

Deliverables:

- Linux CI for formatting, clippy, tests, docs, differential tests, and failure tests;
- release-mode build and smoke test;
- versioned binary archive and SHA-256 checksum;
- generated shell completions if support is low-risk;
- reproducible demo repository/script;
- complete limitations and recovery runbook;
- changelog and release notes;
- storage-size reporting and an explicit no-silent-prune statement;
- audit that `git push --mirror` cannot expose `.git/git2/`;
- dependency/license review;
- benchmark baselines for adoption, status, and restore on representative repositories.

Release gate:

- all earlier exit criteria pass on a clean Linux runner;
- no ignored, flaky, or placeholder test covers a release claim;
- no known data-loss bug remains in supported scope;
- the CLI refuses unsupported states before mutation;
- a new user can follow the documented disposable demo exactly;
- the release is labeled experimental and not recommended for important repositories.

## Deferred v0.2 themes

These are sequenced after the local undo proof:

- clone/fetch/push through the installed Git executable;
- proof that recovery data never crosses network operations;
- native macOS tests and filesystem semantics;
- Windows path/mode/recovery design and native CI;
- recovery storage inspection and explicit garbage collection;
- better operation selection, unique abbreviations, and redo UX;
- performance improvements such as streaming CAS capture;
- selected support for bare repositories or linked worktrees only if recovery semantics are complete.

## Explicitly later

- native Git wire protocols;
- merge, rebase, cherry-pick, stash, and bisect;
- submodule recovery;
- sparse/partial clone recovery;
- SHA-256 Git repositories;
- signing and verification UI;
- hosting-service integration;
- a stable public Rust library API.

## Cross-cutting work required in every milestone

### Documentation

- Update README status and CLI reference.
- Update compatibility matrix.
- Update recovery format for disk changes.
- Record important decisions.
- Add a changelog entry.

### Verification

- Unit tests for parsing and state transitions.
- Differential tests against Git.
- Corruption and refusal tests.
- Byte-path tests on Unix.
- Failure-injection tests for every new mutation.
- `git fsck` after object/ref/index changes.

### Review questions

- Can this command lose state after process or machine interruption?
- Does it capture everything it may overwrite?
- Can an ordinary Git process race it?
- Can any path escape the repository or follow an unsafe symlink?
- Does it preserve ignored/out-of-scope state?
- Does recovery metadata remain private?
- Is the behavior still honest when a supported limit is exceeded?

## Definition of done

A feature is done only when:

1. Its supported and unsupported behavior is explicit.
2. The core logic is separate from CLI presentation.
3. Tests compare compatible behavior with Git where applicable.
4. Byte paths and non-text content are considered.
5. Corruption and interruption behavior is tested.
6. Documentation matches the executable.
7. Formatting, clippy, tests, and docs pass with warnings denied.
8. No safety-critical TODO is hidden behind a successful happy-path demo.
