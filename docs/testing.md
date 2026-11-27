# Testing strategy

## Objective

Tests exist to prove Git interoperability and recovery safety, not merely Rust implementation details. The test strategy therefore uses ordinary Git as an external oracle, byte-level format tests for private recovery data, and refusal/failure tests for unsafe states.

## Quality gates

Run the same gates before every handoff or commit:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Tests that invoke Git require `git` on `PATH`. They create isolated temporary repositories and do not need network access or global author configuration.

## Current test layers

### Unit tests

Unit tests live beside core modules and cover deterministic encodings and state helpers.

Current areas:

- known Git hash for the empty blob;
- canonical object parse/serialize round trip;
- byte commit messages and multiline headers;
- non-UTF-8 tree names and gitlink mode representation;
- recovery view encode/decode;
- recovery operation and CAS deduplication;
- malformed/truncated recovery record refusal;
- exclusive advisory locking;
- byte-safe porcelain generation.

### Differential object tests

`crates/git2-core/tests/object_interop.rs` compares git2 with the installed Git executable.

Current proofs:

- Git computes the same blob ID as git2;
- Git reads a loose object written by git2;
- git2 reads a loose object written by Git;
- git2 reads a packed object after `git gc --prune=now` removes its loose form.

### Repository adoption tests

`crates/git2-core/tests/adoption.rs` exercises disposable repositories.

Current proofs:

- tracked and non-ignored untracked bytes are captured;
- ignored files are excluded;
- raw index and refs are represented;
- oversized files refuse before sidecar creation;
- known in-progress Git state is refused;
- external changes append exactly one correctly linked operation;
- unchanged repeated imports append no operation.

### Differential status tests

`crates/git2-core/tests/status_interop.rs` and `crates/git2-cli/tests/cli.rs` compare `git2` porcelain bytes with `git -c diff.renames=false status --porcelain=v1 --untracked-files=all` (and `-z`) across:

- staged additions, modifications, deletions, executable-bit flips, and symlink/file type transitions (`T `, ` T`, `TT`);
- unstaged modifications, deletions, executable-bit flips, type transitions, and untracked files;
- tracked-before-untracked ordering parity;
- nested `.gitignore` negations, global `core.excludesFile`, unborn `HEAD`, detached `HEAD`, and non-UTF-8 / tab / newline filenames through `git2 status -z`.

### Transaction and crash-recovery tests

`crates/git2-core/tests/transaction.rs` exercises the 4-phase pending journal (`run`, `pending`, `recover`, `rollback`) using deterministic named `Failpoint` hooks and real child processes terminated via `std::process::abort()`:

- no-op synthetic mutation (`git2:synthetic-noop`);
- stale-plan refusal, automatic `external-git` chaining, and advisory lock contention;
- real child `abort()` across every failpoint (`BeforePendingWrite`, `AfterPrepare`, `BeforeLockPublish(i)`, `AfterLockPublish(i)`, `BeforeAction(i)`, `AfterAction(i)`, `BeforeFinalize`, `AfterOperationWrite`, `AfterHeadSelection`, `BeforePendingCleanup`, plus `RecoverAfterHeadSelection` double-crash);
- staged-inode lock attribution recovery for `index.lock` and `refs/heads/*.lock`, and refusal leaving foreign locks byte-identical;
- pruned Git object rehydration, checked directory/worktree/index/ref mutations, `git fsck --full`, and clean `pending/` / lock cleanup;
- injected `Error::Io` rollback verification and corrupt/truncated/missing `pending` refusal tests.

### Restore planning and execution tests

`crates/git2-core/tests/restore.rs` tests verified view restoration against ordinary Git:

- read-only `--dry-run` planning with single-line byte-safe path escaping (including newlines and invalid UTF-8) and zero writes or history changes;
- refusal without writes on ignored children blocking directory replacement, missing/corrupt CAS payloads, differing ignored files, and ambiguous ignored-to-untracked rule transitions;
- dual current/target ignore-profile evaluation across `.gitignore` changes and force-tracked-to-ignored / identical-ignored-to-tracked reclassifications;
- end-to-end byte-level round trips after `git gc --prune=now` deletes staged-only blobs, packed-ref targets, and loose branch targets, verifying `git fsck --full` and intact ignored files;
- file ↔ directory worktree swaps and loose-ref directory/file (`topic` ↔ `topic/sub`) swaps.

### CLI integration and crash-recovery tests

`crates/git2-cli/tests/cli.rs` launches the built binary and verifies:

- adoption creates a baseline and `oplog` emits full 64-character view IDs;
- `restore --dry-run`, `restore`, and clean `recover --status` work end to end;
- repeat adoption and legacy format 1/2 stores are refused;
- debug object hashing matches Git.

`crates/git2-cli/tests/crash_recovery.rs` exercises the built `git2` binary with debug-only `GIT2_TEST_FAILPOINT` (`abort:*` and `io:*`) injection:

- interruption mid-swap during directory replacement recovered via `git2 recover` and verified with `git fsck --full`;
- double crash (`after_head_selection` and `recover_after_head_selection`) resolving idempotently with one `oplog` restore entry;
- injected post-mutation I/O error rolled back cleanly via `git2 recover --rollback`;
- read-only `git2 recover --status` and pending `git2 oplog` on an interrupted layout without mutating `.git/git2/`;
- pre-existing foreign `.git/index.lock` refusal and pending `--dry-run` reporting without writes.

### Native reset and undo tests

`crates/git2-core/tests/reset_undo.rs` verifies `reset` and `undo` against ordinary Git:

- differential comparison against `git reset --soft|--mixed|--hard` (branch ref, detached `HEAD`, `HEAD~N`, local branches, and 40-hex commit IDs), verifying `git ls-files --stage -z`, `git status --porcelain=v1 -z`, `git fsck --full`, and unchanged `ORIG_HEAD` / reflogs;
- Option A non-ignored untracked collision overwrite + `undo` recovery vs refusal of ignored obstructions and invalid revisions / submodule gitlinks / unborn branches;
- full Milestone 6 recovery demo (staged vs worktree edit, deletion, executable bit, symlink, non-UTF-8 path, untracked file, ignored file → `reset --hard` → `git status` stat refresh + `git fsck --full` → `undo --dry-run` → `undo` restoring exact raw pre-reset index and bytes → `undo` redo);
- semantic divergence checks plus refusal of `adopt`, `checkpoint`, and real `external-git` operations with `git2 restore --view` guidance.

`crates/git2-cli/tests/reset_undo_cli.rs` verifies end-to-end CLI workflows and debug-only `GIT2_TEST_FAILPOINT` process kills and I/O rollback for both `git2 reset --hard` and `git2 undo`.

### Staging (`git2 add`) tests

`crates/git2-core/tests/stage.rs` tests literal staging against `git add` (`git ls-files --stage -z`, `git diff --cached --raw -z`, and `git fsck --full`):

- new, modified, deleted, empty, binary (`\0`), executable (`0755`), and symlink entries, plus `git2 undo` reverting `.git/index`;
- literal filenames (`-flag`, `[bracket].txt`, `star*.txt`, embedded newlines, and non-UTF-8 bytes) staged from subdirectories without pathspec globbing;
- nested `.gitignore` negation (`!keep.log`) alongside ignored file/directory refusal with zero index writes;
- `.gitattributes` CRLF conversion matching Git blob IDs vs active `filter=<name>` refusal without running external commands;
- `--all` on unborn repositories, detached `HEAD`, and pre-existing foreign `.git/index.lock` refusal.

`crates/git2-cli/tests/stage_cli.rs` verifies `git2 add -- -dash.txt`, subdirectory invocation, `--all`, `undo`, and debug-only `GIT2_TEST_FAILPOINT` process kills and I/O rollback across `.git/index.lock` writes.

### Commit and log tests

`crates/git2-core/tests/commit_log.rs` tests `commit` and `log` against ordinary Git (`git write-tree`, `git cat-file -p`, `git rev-parse`, and `git fsck --full --strict`):

- root commits on unborn branches, follow-up commits, multiline & normalized messages, and all-deleted empty-tree commits;
- Git canonical tree entry sorting (`a-file` < `a/` < `a_file`), nested trees, executables, symlinks, and non-UTF-8 filenames;
- detached `HEAD`, symbolic branch aliases (`HEAD -> refs/heads/alias -> refs/heads/main`), and packed parent resolution;
- refusals before mutation on blank/NUL messages, empty tree deltas, `INTENT_TO_ADD`, executable commit hooks (`core.hooksPath`), `commit.gpgSign`, unsupported `i18n.commitEncoding`, and foreign ref locks;
- `git2 undo` / redo across both normal and root commits, plus bounded `log::walk` limits and strict erroring on missing/corrupt parents.

`crates/git2-cli/tests/commit_log_cli.rs` verifies end-to-end CLI `commit -m`, `log -n`, `GIT_AUTHOR_*`/`GIT_COMMITTER_*`/`GIT_*_DATE` precedence, `undo`/`oplog`, and `GIT2_TEST_FAILPOINT` process-kill & rollback recovery.

### Release-readiness tests

`crates/git2-cli/tests/release_readiness.rs` verifies:

- `git push --mirror` to a bare remote never transfers `.git/git2/`, `refs/git2/*`, or staged-only/recovery-only objects, and the remote passes `git fsck --full --strict`;
- executing the compiled `git2` binary with `PATH=""` across `adopt`, `status -z`, `add --all`, `commit -m`, `log -n`, `checkpoint`, `reset --hard`, `undo`, `restore --dry-run`, `oplog`, and `recover --status` succeeds with zero runtime dependency on `git`.

## Rules for new tests

### Use Git as the oracle

If Git defines the behavior, build equivalent disposable repositories, run both implementations, and compare durable state or machine-readable bytes. Avoid comparing human prose when a porcelain or object-level check exists.

Useful oracles include:

```sh
git hash-object
git cat-file
git status --porcelain=v1 -z
git diff --cached --raw -z
git ls-files --stage -z
git rev-parse
git show --raw
git fsck --full
```

### Assert durable results

For a mutation, checking exit status is insufficient. Reopen the repository and assert:

- Git sees the expected refs and history;
- index bytes/entries are valid;
- worktree bytes and modes are correct;
- `git fsck --full` succeeds;
- recovery HEAD selects the expected operation;
- operation parent/before/after links are correct;
- the pre-operation view remains readable.

### Keep paths byte-safe

Unix path fixtures should include:

- invalid UTF-8;
- spaces and tabs;
- leading dash;
- newline;
- nested directories;
- symlinks;
- executable regular files.

Use NUL-delimited Git interfaces and compare bytes. Do not use `String` to carry arbitrary paths.

### Test refusal before mutation

Every unsupported feature needs a fixture that records relevant Git/worktree bytes before the command, asserts failure, and proves those bytes did not change.

### Avoid ignored tests

A future test may be checked in only when it compiles and runs. Do not use `#[ignore]`, `todo!`, or `unimplemented!` as a roadmap substitute. Put planned cases in the roadmap until an implementation slice begins.

## Required staging matrix

The staging milestone must differentially cover:

| Dimension | Cases |
| --- | --- |
| Change | add, modify, delete, type change |
| Content | empty, text, binary, large-near-limit |
| Mode | regular, executable, symlink |
| Path | ASCII, invalid UTF-8, newline, nested, leading dash |
| Selection | explicit path, directory/pathspec, `--all` |
| Ignore | root, nested, info exclude, global exclude, forced add |
| Attributes | CRLF, executable, clean filter |
| Repository | unborn HEAD, normal HEAD, detached HEAD |
| Failure | index lock held, process interruption, stale pending state |

Compare `git ls-files --stage -z`, cached diff bytes, object contents, and status output.

## Required commit matrix

The commit milestone must cover:

- root and normal commits;
- nested trees;
- multiple parents only when deliberately supported;
- empty message policy and multiline messages;
- author/committer name, email, timestamp, and timezone;
- executable and symlink tree entries;
- detached and symbolic HEAD;
- concurrent ref movement;
- packed object inputs;
- reflog behavior;
- interruption between object, ref, operation, and recovery HEAD writes.

Every fixture should end with `git fsck --full` and ordinary `git log`/`git cat-file` checks.

## Required restore/undo matrix

A view round trip must include all supported state at once:

- symbolic or detached HEAD;
- loose and packed refs;
- staged and unstaged changes;
- deleted tracked paths;
- non-ignored untracked paths;
- ignored files that must remain untouched;
- regular, executable, symlink, empty, binary, and non-UTF-8 paths;
- current extra eligible paths that must be removed;
- a target path whose parent shape changed from file to directory or vice versa.

Compare raw HEAD, raw index, logical refs, exact bytes, file kinds, executable bits, and eligible path absence.

## Failure injection plan

Before the first mutation, add deterministic named failpoints around:

1. pre-view CAS writes;
2. pre-view record write;
3. pending record write and sync;
4. each Git lock acquisition;
5. Git object/index/ref mutation;
6. after-view CAS and view write;
7. operation write;
8. recovery HEAD update;
9. pending cleanup.

The harness should launch the real CLI in a child process, trigger one failpoint, terminate or force the injected error, reopen through both Git and git2, and assert the documented reconciliation outcome. In-process error tests alone do not model descriptor release and process death.

Machine/power-loss durability cannot be perfectly simulated by process termination. Release testing should also document filesystem assumptions and, where practical, run crash-loop tests in a disposable filesystem/container.

## Corruption testing

For every binary recovery record, cover:

- wrong magic/version;
- invalid markers/kind codes;
- truncation at every field class;
- oversized lengths/counts;
- whole record files over the configured/read limit;
- encoder input that would exceed decoder limits;
- invalid digest text in HEAD;
- name/content digest mismatch;
- trailing bytes;
- missing referenced operation, view, or CAS object;
- immutable destination containing different bytes.

Property-based or fuzz testing is a useful follow-up after the pending format stabilizes. It does not replace curated semantic fixtures.

## CI plan

The active Linux CI workflow (`.github/workflows/ci.yml`) runs on every push to `main` and pull request:

1. logs `rustc --version`, `cargo --version`, and `git --version`;
2. formatting check (`cargo fmt --all -- --check`);
3. clippy for workspace/all targets with warnings denied (`cargo clippy --workspace --all-targets -- -D warnings`);
4. all workspace unit, differential, privacy, `PATH=""`, and process-kill tests (`cargo test --workspace --all-targets`);
5. warning-denied rustdoc (`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`);
6. release build (`cargo build --release --workspace`);
7. disposable-repository smoke demo (`GIT2_BIN="$PWD/target/release/git2" bash scripts/demo.sh`).

Local dependency/license auditing (`cargo metadata`) and release tarball + SHA-256 packaging (`scripts/package-release.sh`) are documented in [Experimental v0.1 release notes](release-v0.1.md). macOS and Windows jobs become required only when those platforms enter the supported matrix.

## Test review checklist

- Does the test fail for the intended bug, not an incidental string?
- Does it compare bytes where bytes matter?
- Does it inspect state after reopening?
- Does it verify both Git compatibility and recovery reachability?
- Does it cover cleanup and absence, not only created files?
- Is the fixture isolated from global Git config where relevant?
- Is the error-path assertion proving no mutation?
- Is the test deterministic without network access?
