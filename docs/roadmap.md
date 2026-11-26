# Roadmap

Last reconciled with the implementation: 2026-11-26.

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
| 2. Adoption and immutable history | Done | Zlib-compressed format 3 CAS, views, operations, lock, external import, object retention, explicit checkpoint |
| 3. Status | Done | Differential two-column porcelain and `-z` output |
| 4. Mutation transaction foundation | Planned | Next milestone; 4-phase constrained-action journal + synthetic crash proof |
| 5. Restore planning, apply, and recovery | Planned | `restore --view [--dry-run]`, `recover [--status]`, read-only pending `oplog` |
| 6. Native reset and non-destructive undo proof | Planned | `reset`, semantic-index `undo`/redo, interruption tests + **stop/review gate** |
| 7. Git-style staging | Planned | `git2 add` (literal paths, `--all`, built-in EOL) after post-proof review gate |
| 8. Commit and history | Planned | `commit -m` and bounded first-parent `log` |
| 9. Status completion, docs, and experimental v0.1 release | Planned | Complete supported status matrix, Linux CI, mirror-push privacy check, demo, package, checksums |

Explicit `git2 checkpoint` capture, Git object retention, and zlib-compressed format 3 CAS are already done under Milestone 2, alongside the earlier adoption/capture work. They do not complete a mutation transaction: Milestone 4 remains the next planned milestone, and no mutating command ships before its pending-transaction foundation passes. After Milestone 6 proves `reset --hard` → `undo` → `redo` end to end under process-kill testing, the plan pauses at an explicit review gate before starting Milestones 7–9.

## Milestone 0 — repository rescue

**Status: Done**

Delivered:

- removed private conversation transcripts and abandoned planning artifacts;
- removed speculative backend/workflow crates and placeholder commands;
- reduced the workspace to `git2-core` and `git2-cli`;
- pinned Rust (now 1.98.0) and workspace lint policy;
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
- CLI operation log;
- checked worktree capture, format 3 zlib-compressed CAS records, persisted limits, and owner-private storage;
- retention of the Git object closure from saved physical loose and packed refs (including shadowed values and peeled OIDs), HEAD, concrete index blobs, and valid cache-tree IDs, following tag/commit/tree edges without public refs or object writes;
- explicit `git2 checkpoint` on an adopted repository, reporting the full 64-hex view ID, logging `git2:checkpoint` on a changed save and reusing the same view with no new operation when unchanged.

Exit criteria met:

- ignored files are demonstrably excluded;
- oversized files refuse adoption before sidecar creation;
- known in-progress Git state is refused;
- imported external changes form a correct parent/before/after chain;
- corrupt/truncated records (including truncated/trailing-byte zlib CAS entries), legacy format 1/2 stores, and concurrent git2 lock acquisition are tested;
- retained objects survive ordinary Git pruning of shadowed refs and staged blobs, and unchanged checkpoints reuse the current view.

Known debt carried forward:

- capture consistency against concurrently running ordinary Git is not proven; git2 does not monitor plain Git and checkpoints are explicit;
- retention is bounded (128 MiB per object payload, 1,000,000 distinct objects, pack delta depth 64 with a 128 MiB aggregate instruction budget, plus the whole-view limit) and refuses REUC indexes and historical gitlink trees;
- no streaming capture or garbage collection;
- no restore or pending-transaction journal exists yet, and captures are not backups.

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

Explicit checkpoint capture, object retention, and format 3 zlib-compressed CAS were completed under Milestone 2; they do not provide a mutation transaction. This milestone is the next planned work before restore or any user-facing Git mutation.

Estimated effort: 2–4 focused weeks.

Deliverables:

1. Define and document a versioned pending-transaction record using a constrained 4-phase state machine (`Prepared -> Applying -> Finalizing -> Committed`) over a small fixed vocabulary of primitive checked actions (`RehydrateObject`, `WriteWorktreeEntry`, `RemoveWorktreeEntry`, `EnsureDir`, `RemoveEmptyDir`, `WriteIndex`, `RemoveIndex`, `WriteRef`, `RemoveRef`, `WritePackedRefs`, `WriteHead`).
2. Add `RecoveryStore` / transaction APIs to create, advance, read, and clear one pending transaction under the advisory lock, with same-filesystem staged-inode lock attribution.
3. Capture a pre-operation view and sync it before any mutation callback runs.
4. Add a transaction runner that finalizes an after-view and immutable operation only on success.
5. Reconcile pending state idempotently on startup, including survival of a second crash after recovery `HEAD` selection.
6. Detect or refuse concurrent ordinary-Git changes across capture/mutation boundaries.
7. Add deterministic failpoints after every durable write and before/after each mutation boundary.
8. Build process-kill tests that reopen the repository and prove an actionable state remains with zero manual repair.
9. Apply the existing bounded-record, persisted-limit, and private-storage rules to journal data.

Exit criteria:

- a no-op synthetic mutation produces a valid before/after operation;
- killing the process at every failpoint never loses the pre-operation view;
- restart reconciliation is idempotent across repeated interruptions;
- lock contention fails immediately without partial mutation, and unknown Git locks remain untouched;
- pending corruption is refused with no Git-state write;
- every newly written view/operation is guaranteed readable under the same format limits;
- the format and recovery procedure are documented byte for byte.

Stop condition: do not begin user-facing restore, index, or ref mutation until this gate passes.

## Milestone 5 — restore planning, apply, and recovery

**Status: Planned**

Estimated effort: 3–5 focused weeks after Milestone 4.

Commands:

```text
git2 restore --view <recovery-id> --dry-run
git2 restore --view <recovery-id>
git2 recover [--status]
```

Deliverables:

- verify every target operation/view/CAS record and retained Git object before mutation;
- rehydrate missing required Git objects before publishing refs or index;
- evaluate both current and target ignore rules (including planned `.gitignore` changes) and refuse ignored-child directory obstructions before writing;
- display a byte-safe `--dry-run` plan for HEAD, refs, index, and worktree creates/replacements/removals without modifying Git state or sidecar history;
- snapshot current state before restore and write files without following symlinks or escaping the worktree;
- preserve ignored files and make multi-path restore restartable through pending phases;
- expose `git2 recover [--status]` and read-only `git2 oplog` when a transaction is pending;
- append `git2:restore` operations.

Exit criteria:

- restoring a captured view after `git gc` rehydrates staged-only and shadowed objects and reproduces modeled bytes, modes, symlinks, refs, HEAD, and raw index exactly;
- extra eligible files are removed while ignored files remain untouched;
- non-UTF-8 paths, symlinks, executable bits, and deleted tracked paths round-trip;
- restore interruption at every durable or destructive boundary reconciles cleanly without manual repair;
- traversal and symlink-attack tests cannot write outside the worktree.

## Milestone 6 — native reset and non-destructive undo proof (review gate)

**Status: Planned**

Estimated effort: 3–5 focused weeks after Milestone 5.

Commands:

```text
git2 reset [--soft|--mixed|--hard] <commit>
git2 undo [--dry-run]
```

Deliverables:

- resolve full commit IDs, `HEAD`, and bounded `HEAD~N` (up to 10,000 first-parent steps);
- implement soft, mixed (default), and hard reset semantics for supported repositories without touching `ORIG_HEAD` or reflogs;
- preserve unrelated untracked paths and refuse out-of-scope obstructions;
- undo the latest eligible completed git2 mutation by restoring its `before` view and appending a new `git2:undo` operation (undoing the undo to redo);
- compare current index semantic entries (paths, stages, modes, OIDs, and flags) against the recorded after-view so plain `git status` stat-cache refreshes do not falsely block `git2 undo`, while real staged/ref/worktree divergence still refuses and restored pre-reset index bytes match exact raw bytes;
- provide a complete recorded crash-recovery demonstration.

Core demonstration:

1. Adopt a disposable repository containing tracked, staged, modified, deleted, executable, symlink, non-UTF-8, non-ignored untracked, and ignored state.
2. Perform a destructive `git2 reset --hard`.
3. Verify ordinary Git (`git status`, `git fsck --full`) accepts the reset state.
4. Run `git2 undo`, then `git2 undo` again to redo.
5. Verify HEAD, refs, raw index, all eligible bytes, modes, symlinks, and absences equal the expected views while ignored files remain untouched.
6. Verify the reset, undo, and redo operations all remain in `oplog`.
7. Repeat with forced termination at every transaction phase.

Exit criteria:

- soft/mixed/hard results match Git for the supported matrix;
- the demonstration passes automatically and manually, including stat-only `git status` before `undo`;
- undo never deletes an operation or immutable record, and undo itself can be undone;
- staged/ref/worktree divergence is detected rather than silently overwritten;
- **Stop & Review Gate:** pause after this milestone passes to review the recovery proof before starting Milestones 7–9.

## Milestone 7 — Git-style staging

**Status: Planned (after Milestone 6 review gate)**

Estimated effort: 3–5 focused weeks.

Command:

```text
git2 add [--all] [--] [<path>...]
```

Deliverables:

- treat path arguments as literal repository- or cwd-relative files and directories (supporting `--` for flag-like names, with no pathspec magic/globs);
- stage new/modified files and deletions, plus `--all` repository-wide;
- compute built-in text/EOL conversions through `gix`, preserve executable and symlink modes, and refuse active external filters or unsupported conversions where they would apply;
- obey ignore rules (no forced addition of ignored files in v0.1);
- use Git index lockfile semantics wrapped in the transaction runner and append `git2:add` operations.

Differential matrix:

- new/modified/deleted files, empty files, and binary content;
- executable-bit changes and symlinks;
- literal filenames with leading `-`, newline, brackets, and non-UTF-8 bytes;
- nested `.gitignore` and ignored-path refusal;
- `.gitattributes` and CRLF built-in conversions vs active external-filter refusal;
- partial literal paths and `--all`, unborn/detached HEAD, foreign `index.lock` refusal, and interrupted index write.

Exit criteria:

- Git (`git ls-files --stage -z`, `git diff --cached --raw -z`) and git2 produce equivalent index entries for the supported matrix;
- `git fsck` succeeds;
- every successful add has a complete recoverable before/after operation;
- every failpoint leaves a valid index or an actionable pending transaction.

## Milestone 8 — commit and bounded log

**Status: Planned**

Estimated effort: 2–4 focused weeks.

Commands:

```text
git2 commit -m <message>
git2 log [--max-count <n>]
```

Deliverables:

- build canonical trees recursively from stage-zero index entries;
- reject unmerged index stages, blank commit messages, empty tree changes, and active hooks/signing/unsupported config;
- resolve author/committer identity and local time/timezone from Git env overrides and config;
- create root and single-parent commits, preserving byte-safe commit messages internally;
- update symbolic or detached HEAD with transactional ref compare-and-swap (leaving reflogs unchanged in v0.1);
- wrap commit/ref updates in the mutation transaction and append `git2:commit` operations;
- walk bounded first-parent history from `HEAD` (default 50, max 10,000), reporting missing/corrupt parents rather than silently truncating.

Exit criteria:

- `git log`, `git cat-file`, and `git fsck` accept every git2 commit fixture;
- tree IDs match Git for equivalent index state;
- ref races fail without overwriting another writer;
- interrupted commit/ref updates reconcile through pending state.

## Milestone 9 — status completion, docs, and experimental v0.1 release

**Status: Planned**

Estimated effort: 3–5 focused weeks.

Deliverables:

- close remaining Milestone 3 status follow-up coverage (executable bits, symlinks, type transitions, non-UTF-8/newline `-z`, nested/global excludes, unborn/detached HEAD) with rename detection disabled;
- reconcile all user documentation and relative links, and stop warning that snapshots are non-restorable only after Milestone 6 passes;
- Linux CI for formatting, warning-denied clippy/docs, tests, differential fixtures, and process-kill crash suites;
- release-mode build, smoke test with no Git on `PATH`, versioned binary archive, and SHA-256 checksum;
- mirror-push privacy test proving `.git/git2/` and staged-only objects never transfer via `git push --mirror`;
- reproducible demo script, limitations/recovery runbook, storage-size reporting, dependency/license review, and benchmark baselines.

Release gate:

- all earlier exit criteria pass on a clean Linux runner;
- no ignored, flaky, or placeholder test covers a release claim;
- no known data-loss bug remains in supported scope;
- the CLI refuses unsupported states before mutation;
- a new user can follow the documented disposable demo exactly;
- the release is labeled experimental and not recommended for important repositories; do not publish without separate authorization.

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
