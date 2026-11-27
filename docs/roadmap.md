# Roadmap

Last reconciled with the implementation: 2026-11-27.

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

At that pace, the planned v0.1 milestones 0–9 are implemented on Linux; only explicit release tagging/publication and post-v0.1 (`v0.2+`) work remain.

## Current summary

| Milestone | Status | Product evidence |
| --- | --- | --- |
| 0. Repository rescue | Done | Honest two-crate workspace, private artifacts removed, clean gates |
| 1. Git object interoperability | Done | Loose round trips, packed reads, byte-safe commit/tree tests |
| 2. Adoption and immutable history | Done | Zlib-compressed format 3 CAS, views, operations, lock, external import, object retention, explicit checkpoint |
| 3. Status | Done | Differential two-column porcelain and `-z` output |
| 4. Mutation transaction foundation | Done | 4-phase constrained-action journal, staged-inode lock attribution, and child process-kill proof |
| 5. Restore planning, apply, and recovery | Done | `restore --view [--dry-run]`, `recover [--status \| --rollback]`, pending read-only full-hex `oplog`, dual ignore profiles, pruned-object rehydration, CLI crash tests |
| 6. Native reset and non-destructive undo proof | Done | `reset [--soft\|--mixed\|--hard]`, semantic-index `undo [--dry-run]` / redo, full core & CLI crash proof |
| 7. Git-style staging | Done | `git2 add [--all] [--] [<path>...]`, built-in EOL conversion, external-filter & ignore refusal, differential & crash suites |
| 8. Commit and history | Done | Native `git2 commit -m` via the 4-phase pending journal (`undo`-capable) and bounded first-parent `git2 log [-n <n>]` |
| 9. Status completion, docs, and experimental v0.1 release | Done | Expanded status matrix, Linux CI, mirror-push privacy check, empty-`PATH` check, demo, package script, runbook, v0.1 readiness report |

All v0.1 milestones (0–9) are implemented and covered by the workspace test suites (`git2 adopt`, `checkpoint`, `status`, `oplog`, `restore`, `recover`, `reset`, `undo`, `add`, `commit`, and `log`). Creating an actual public release tag or publishing packages remains gated on separate maintainer authorization.

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

**Status: Done**

Delivered:

1. Defined and documented byte-for-byte versioned `pending/active` (`git2-pending\0\x01`) and `pending/plan` (`git2-plan\0\x01`) records driving a 4-phase state machine (`Prepared -> Applying -> Finalizing -> Committed`) over 11 checked primitive actions (`RehydrateObject`, `WriteWorktreeEntry`, `RemoveWorktreeEntry`, `EnsureDir`, `RemoveEmptyDir`, `WriteIndex`, `RemoveIndex`, `WriteRef`, `RemoveRef`, `WritePackedRefs`, `WriteHead`).
2. Added `RecoveryRepository`, `pending`, `run` / `run_with_hook`, `recover` / `recover_with_hook`, and `rollback` under the exclusive advisory lock, plus checked descriptor-relative worktree/directory write primitives in `paths.rs`.
3. Implemented same-filesystem lock staging (`pending/stage_<16hex>_<index>`) and `(st_dev, st_ino)` ownership attribution before hard-linking Git locks without replacement, leaving foreign locks byte-identical.
4. Enforced pre-write durability, automatic `external-git` chaining or stale-plan refusal, pending guards on `checkpoint` and `import_external_changes`, and idempotent startup reconciliation (including double-crash survival after `HEAD` selection).
5. Added deterministic failpoints and real child `std::process::abort()` crash-recovery tests, injected I/O rollback tests, and pending-corruption refusal tests in `crates/git2-core/tests/transaction.rs`.

Exit criteria met:

- a no-op synthetic mutation produces a valid before/after operation;
- killing a child process at every failpoint never loses the pre-operation view;
- restart reconciliation is idempotent across repeated interruptions;
- lock contention fails immediately without partial mutation, and unknown Git locks remain untouched;
- pending corruption is refused with no Git-state write;
- every newly written pending/plan/operation record is bounded and readable under the same format limits;
- the format and recovery procedure are documented byte for byte.

## Milestone 5 — restore planning, apply, and recovery

**Status: Done**

Commands:

```text
git2 restore --view <recovery-id> --dry-run
git2 restore --view <recovery-id>
git2 recover [--status | --rollback]
```

Delivered:

- read-only verified restore planner (`restore::plan`, `MutationPlan::changes`) with single-line byte-safe `--dry-run` preview and zero sidecar writes or lock acquisition;
- full target view, CAS payload, and retained Git object closure verification up front, rehydrating pruned objects before publishing refs or index;
- dual current/target ignore-profile evaluation (`gix::ignore::Search` over global/XDG excludes, `.git/info/exclude`, and live/target `.gitignore`) refusing ignored-child directory obstructions and ambiguous ignored-to-untracked transitions before writes while allowing safe non-destructive reclassifications;
- file ↔ directory worktree swaps, loose-ref D/F swaps with empty ancestor ref directory pruning, and pre-existing foreign lock refusal;
- `git2 recover [--status | --rollback]` and pending-aware read-only `git2 oplog` emitting full 64-hex IDs, plus `crates/git2-core/tests/restore.rs` and `crates/git2-cli/tests/crash_recovery.rs`.

Exit criteria met:

- restoring a captured view after `git gc --prune=now` rehydrates staged-only and shadowed objects and reproduces modeled bytes, modes, symlinks, refs, HEAD, and raw index exactly;
- extra eligible files are removed while ignored files remain untouched;
- non-UTF-8 paths, symlinks, executable bits, and deleted tracked paths round-trip;
- restore interruption at durable and destructive boundaries reconciles cleanly without manual repair;
- traversal and symlink-attack tests cannot write outside the worktree.

## Milestone 6 — native reset and non-destructive undo proof (review gate)

**Status: Done — at stop/review gate**

Estimated effort: 3–5 focused weeks after Milestone 5.

Commands:

```text
git2 reset [--soft|--mixed|--hard] [<commit>]
git2 undo [--dry-run]
```

Delivered:

- resolve `HEAD`, full 40-hex commit IDs, local branch names (`refs/heads/<name>`), and bounded `~N` (up to 10,000 first-parent steps), refusing unborn targets, unsupported syntax, non-commits, and submodule gitlinks;
- implement soft, mixed (default), and hard reset semantics natively (`git2-core::reset`) without touching `ORIG_HEAD` or reflogs, overwriting colliding non-ignored untracked files recoverably while preserving unrelated untracked/ignored paths and refusing ignored obstructions;
- undo the latest eligible completed git2 mutation (`git2-core::undo`) via `restore` without rewriting history, appending `git2:undo` (undoing an undo redoes), and refusing `adopt`, `checkpoint`, and semantic `external-git` entries with `git2 restore --view` guidance;
- semantic index entry comparison across stat-cache refreshes (`git status` and stat-only `external-git` imports) while still detecting staged entry/mode/OID/flag divergence and restoring exact raw pre-operation `.git/index` bytes;
- complete core differential/demo suite (`crates/git2-core/tests/reset_undo.rs`) and CLI crash-recovery proof (`crates/git2-cli/tests/reset_undo_cli.rs`).

Exit criteria met:

- soft/mixed/hard results match Git (`git ls-files --stage -z`, `git status --porcelain=v1 -z`, `git fsck --full`) for the supported matrix;
- the release demonstration passes across worktree/index state, stat-only `git status` before `undo`, redo, and process-kill/rollback failpoints;
- undo never deletes an operation or immutable record, and undo itself can be undone;
- staged/ref/worktree divergence is detected rather than silently overwritten;
- **Stop & Review Gate:** pause here to review the Milestone 6 proof before starting Milestones 7–9.

## Milestone 7 — Git-style staging

**Status: Done**

Command:

```text
git2 add [--all] [--] [<path>...]
```

Delivered:

- literal repository- and cwd-relative file/directory staging (`git2-core::stage`) with `--` support for flag-like names and zero pathspec glob/magic expansion;
- stages new, modified, and deleted entries (including D/F replacements and recursive directories) plus `--all` repository-wide;
- built-in text/EOL conversions through `gix::filter` (`text`, `eol`, `core.autocrlf`, `core.eol`), preserving executable and raw symlink modes while refusing active external `filter=` drivers and non-UTF-8 `working-tree-encoding` before mutation;
- enforces effective repository ignore rules (no forced addition of ignored files in v0.1) and appends new loose blobs before transactional `.git/index.lock` index replacement and `git2:add` operation recording.

Exit criteria met:

- Git (`git ls-files --stage -z`, `git diff --cached --raw -z`) and git2 produce matching index entries across binary/empty/exec/symlink/weird-path/nested-ignore/CRLF/unborn/detached cases;
- `git fsck --full` succeeds after staging and recovery;
- every successful `git2 add` records a recoverable before/after operation reversible via `git2 undo`;
- every injected crash and I/O failpoint recovers or rolls back cleanly.

## Milestone 8 — commit and bounded log

**Status: Done**

Commands:

```text
git2 commit -m <message>
git2 log [-n|--max-count <n>]
```

Delivered:

- builds canonical recursive trees bottom-up from stage-zero index entries (`git2-core::commit`), sorting entries with Git's canonical directory-`/` comparison;
- rejects blank/NUL commit messages, unchanged staged trees (`HEAD^{tree}` or empty unborn index), `INTENT_TO_ADD` (`git add -N`), `REUC` records, executable commit hooks (including custom `core.hooksPath`), `commit.gpgSign = true`, unsupported `i18n.commitEncoding`, missing/invalid author/committer identity, and pre-existing foreign locks before mutation;
- resolves author/committer name, email, and timestamp/timezone from `GIT_AUTHOR_*` / `GIT_COMMITTER_*` env vars and Git config;
- creates root and single-parent commits, updating detached `HEAD` or symbolic branch chains (`HEAD -> ref -> leaf`) through the 4-phase pending transaction journal, rehydrating missing trees/commits (including the canonical empty tree) into `.git/objects/` cleanly, and appending reversible `git2:commit` operations (`git2 undo` / redo tested);
- walks bounded first-parent commit history from `HEAD` (`git2-core::log`, default 50, max 10,000), failing cleanly on unborn branches, parent cycles, or missing/corrupt parent commits rather than silently truncating.

Exit criteria met:

- `git log`, `git cat-file`, `git rev-parse HEAD^{tree}`, and `git fsck --full --strict` accept every `git2 commit` fixture;
- tree IDs match `git write-tree` across tricky boundary names, executables, symlinks, non-UTF-8 names, and empty-tree deletions;
- foreign ref locks refuse before mutation;
- interrupted commit/ref updates reconcile or roll back cleanly across every CLI failpoint boundary.

## Milestone 9 — status completion, docs, and experimental v0.1 release

**Status: Done (awaiting separate maintainer authorization before publishing any release tag)**

Delivered:

- expanded `crates/git2-core/tests/status_interop.rs` and `crates/git2-cli/tests/cli.rs` to cover executable-bit flips, symlink & file/type transitions (`T `, ` T`, `TT`), non-UTF-8 + tab + embedded-newline paths through `git2 status -z`, nested `.gitignore` negations & global `core.excludesFile`, unborn `HEAD`, and detached `HEAD` (fixing tracked-before-untracked porcelain sort parity with `git status`);
- added `crates/git2-cli/tests/release_readiness.rs` verifying `git push --mirror` privacy (`.git/git2/`, `refs/git2/*`, and staged-only/recovery-only objects never transfer) and an end-to-end `PATH=""` smoke test proving zero runtime dependency on an installed `git` binary;
- upgraded `.github/workflows/ci.yml` to log toolchain/Git versions, run `fmt`, `clippy`, workspace tests, warning-denied `doc`, `cargo build --release`, and `scripts/demo.sh`;
- added reproducible `scripts/demo.sh`, `scripts/package-release.sh` (versioned Linux tarball + `.sha256` under gitignored `target/release-dist/`), [operations & recovery runbook](runbook.md), and [v0.1 release-readiness dossier](release-v0.1.md) (dependency/license audit and measured baseline timings/storage footprint), and reconciled all project documentation with zero broken relative links.

Release gate:

- all earlier exit criteria pass on a clean Linux runner;
- no ignored, flaky, or placeholder test covers a release claim;
- no known data-loss bug remains in supported scope;
- the CLI refuses unsupported states before mutation;
- a new user can follow the documented disposable demo (`scripts/demo.sh`) directly;
- the release remains explicitly labeled experimental and unsafe for important repositories, and `publish = false` remains set with no GitHub Release or tag published without separate authorization.

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
