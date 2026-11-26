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

`crates/git2-core/tests/status_interop.rs` compares git2 porcelain bytes with `git status --porcelain=v1 --untracked-files=all` for the current supported fixture.

The fixture combines:

- a staged addition;
- a file modified in both index and worktree;
- an unstaged deletion;
- an untracked file.

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

The v0.1 Linux pipeline should run:

1. formatting check;
2. clippy for workspace/all targets with warnings denied;
3. all tests with a pinned Git version recorded in logs;
4. warning-denied rustdoc;
5. release build;
6. disposable-repository smoke demo;
7. failure-injection suite;
8. `cargo metadata` and license/dependency audit;
9. archive checksum generation for release tags.

macOS and Windows jobs become required only when those platforms enter the supported matrix.

## Test review checklist

- Does the test fail for the intended bug, not an incidental string?
- Does it compare bytes where bytes matter?
- Does it inspect state after reopening?
- Does it verify both Git compatibility and recovery reachability?
- Does it cover cleanup and absence, not only created files?
- Is the fixture isolated from global Git config where relevant?
- Is the error-path assertion proving no mutation?
- Is the test deterministic without network access?
