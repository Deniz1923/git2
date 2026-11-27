# Roadmap

Last reconciled with `main`: 2026-11-27.

> [!WARNING]
> `git2` is currently at `0.1.0-alpha.1` (`publish = false`, **experimental and not safe for important repositories**). **v0.1 (Milestones 0–10)** is complete on `main`; **v0.2+** is active development. The experimental warning stays in place until every **v1.0** gate passes.

## How to read this roadmap

This roadmap is an engineering plan organized by release milestone. A milestone is complete only when its commands, recovery invariants, differential Git checks, process-crash/refusal coverage, and documentation all meet its listed exit criteria.

- **Status labels:** **Done** (implemented and verified on Linux CI), **Planned** (designed next), **Future** (later release track).
- **Guiding rules:**
  - Ordinary Git remains the repository format and compatibility oracle.
  - Every new local mutation executes inside the pending-transaction journal (or an explicitly documented conflict-safe delegated wrapper) so interruptions never lose the pre-operation view.
  - Never silently degrade recovery scope, and never silently prune reachable recovery history.
  - Do not scaffold unimplemented CLI subcommands as `todo!()` placeholders.

---

## Completed — v0.1: Local Recovery Core & Delegated Remotes (Milestones 0–10)

All **v0.1** milestones are implemented and covered by the Linux workspace test suite (`cargo test --workspace --all-targets`), `git fsck --full --strict`, privacy checks, and `scripts/demo.sh`. Public release tagging and crate publishing remain disabled (`publish = false`) pending explicit maintainer authorization.

| Milestone | Status | Delivered |
| --- | --- | --- |
| **0. Repository foundation** | Done | Two-crate workspace (`git2-core`, `git2-cli`), Rust `1.98.0`, `unsafe_code = "forbid"`, zero placeholder commands, strict CI gates |
| **1. Git object interoperability** | Done | Byte-safe SHA-1 IDs, canonical framing/hashing, verified loose reads/writes, `gix` packed reads, commits/trees, `debug hash-object` / `cat-object` |
| **2. Private sidecar & checkpoints** | Done | Format `3\n` owner-private storage (`.git/git2/`), zlib SHA-256 CAS, immutable views/operations, advisory writer lock, external import, Git object-closure retention, `git2 adopt` / `checkpoint` / `oplog` |
| **3. Status parity** | Done | Merged `HEAD` → index → worktree model matching Git two-column porcelain and NUL-delimited `-z` output across modes, symlinks, non-UTF-8/tab/newline paths, unborn/detached `HEAD` |
| **4. Mutation transaction journal** | Done | 4-phase journal (`Prepared -> Applying -> Finalizing -> Committed`), 11 checked primitive actions, staged-inode Git lock attribution, idempotent `recover` / `rollback`, child `abort()` crash matrix |
| **5. Verified restore & recovery** | Done | Read-only `git2 restore --view <id> [--dry-run]`, `git2 recover [--status \| --rollback]`, dual ignore-profile checks, pruned-object rehydration, file/dir & ref D/F swaps |
| **6. Native reset & non-destructive undo** | Done | `git2 reset [--soft \| --mixed \| --hard] [<commit>]` (`HEAD`, branches, 40-hex OIDs, `~N`) and semantic-index `git2 undo [--dry-run]` / redo |
| **7. Git-style staging** | Done | `git2 add [--all] [--] [<path>...]` literal CWD-relative staging, built-in EOL conversion, external-filter & ignored-target refusal, transactional index lock |
| **8. Commit & bounded log** | Done | Native `git2 commit -m <msg>` (canonical tree builder, Git identity/date resolution, hook/GPG/encoding/empty/ITA refusals) and bounded first-parent `git2 log [-n <n>]` |
| **9. Experimental v0.1 hardening** | Done | `git push --mirror` privacy verification, `PATH=""` offline smoke test, `scripts/demo.sh`, `scripts/package-release.sh`, [runbook](runbook.md), verification baseline |
| **10. Delegated remotes & networking** | Done | `git2 remote`, `clone` (auto-adopt + `--no-adopt`), undoable `fetch`, `pull` (`--ff-only` default + conflict-recoverable `--merge`/`--rebase`), non-undoable `push`, `.git/git2/pending/delegated`, credential redaction, `REUC` stripping on resolved `recover` |

---

## v0.2 — Everyday Local Git Workflow

Make `git2` practical as a daily local Git CLI without falling back to ordinary `git` for routine inspection, branching, path restores, or stashing—every mutation captured cleanly and reversible via `git2 undo`.

### Milestone 11 — Branches, Switching & Tags

**Status: Planned**

- **Commands:**
  - `git2 branch [-v] | <name> [<start-point>] | -d/-D <name> | -m <old> <new>`
  - `git2 switch [-c|-C <new-branch>] [<branch> | --detach <commit>]` (and evaluate `checkout` ergonomics / explicit conflict-free scope)
  - `git2 tag [-l] | <name> [<commit>] | -a <name> -m <msg> | -d <name>`
- **Deliverables:**
  - Safe local branch & tag ref creation, rename, and deletion inside the 4-phase pending transaction journal.
  - Annotated tag object parsing and serialization in `git2-core::object` (matching `git mktag` / `git fsck --strict`).
  - Dirty-worktree safety rules when switching branches (refusing to overwrite uncommitted worktree/index edits unless safe or fast-forwardable).
- **Exit criteria:**
  - `branch`, `switch`, and `tag` match ordinary Git on refs, index, worktree bytes, and `git fsck --full --strict`.
  - Every mutating branch/switch/tag invocation is cleanly reversible via `git2 undo` and crash-tested at journal boundaries.

### Milestone 12 — Diff, Show, Path Restore & Commit Amend

**Status: Planned**

- **Commands:**
  - `git2 diff [--staged | --cached] [--stat] [<commit> [<commit>]] [--] [<path>...]`
  - `git2 show [<object>]`
  - `git2 restore [--staged] [--worktree] [-s <source>] [--] <path>...` (alongside existing `--view <id>` whole-snapshot restore)
  - `git2 commit --amend [-m <message>] [--no-edit]`
- **Deliverables:**
  - Read-only byte-safe unified diff and object inspection (`blob`, `tree`, `commit`, `tag`) without shelling out.
  - Path-scoped index/worktree restore transactions that still capture complete pre-operation recovery views before modifying files or `.git/index`.
  - Safe `commit --amend` updating `HEAD`/branch atomically while preserving pre-amend commit & tree objects in private retention for `git2 undo`.
- **Exit criteria:**
  - Path-scoped `restore` and `commit --amend` pass differential Git checks and `git2 undo` / redo round trips.
  - Binary files, executables, symlinks, and non-UTF-8 paths render safely in `diff` / `show` and restore byte-for-byte.

### Milestone 13 — Stashing & Worktree Hygiene

**Status: Planned**

- **Commands:**
  - `git2 stash [push [-u] [-m <msg>] | list | show | apply | pop | drop [<stash>]]`
  - `git2 clean [-n|--dry-run] [-f] [-d]`
- **Deliverables:**
  - Compatible stash commits and `refs/stash` management (or clean interoperability with Git's `refs/stash` model) inside pending transactions.
  - Default-dry-run / explicit-`-f` `git2 clean` that snapshots present non-ignored untracked files into `.git/git2/` *before* removing them, turning accidental cleans or `stash drop` mistakes into one-command `git2 undo` recoveries (ignored files remain untouched without `-x` or stay explicitly scoped).
- **Exit criteria:**
  - Dropped stashes and cleaned non-ignored untracked files are demonstrably recovered byte-for-byte via `git2 undo`.
  - Interrupted `stash` and `clean` transactions reconcile cleanly with `git2 recover`.

---

## v0.3 — Recovery Ergonomics, Storage GC & Scale

Turn the raw v0.1 format-3 recovery log into a fast, low-overhead, ergonomically inspectable store with bounded memory usage and explicit disk management.

### Milestone 14 — Recovery UX & Power Workflows

**Status: Planned**

- **Commands & flags:**
  - `git2 redo` (explicit ergonomic counterpart to double-`undo`)
  - Short unambiguous prefixes (minimum prefix length, collision refusal) for operation IDs and view IDs across `restore`, `oplog`, and recovery errors
  - `git2 oplog [-n <n>] [--oneline] [--kind <kind>] [--since <ts>]`
  - `git2 status` optional `-M` / `--find-renames` support matching Git's `R` / `C` porcelain codes
  - Policy & UX design for non-HEAD divergence undo vs explicit target-view restore
- **Deliverables:**
  - Resolved ID-prefix lookup (`git2-core::recovery`) refusing ambiguity cleanly.
  - Clear human/script views of `oplog` transitions (changed file/ref summaries between before/after views).
- **Exit criteria:**
  - Prefix resolution, `redo`, and `oplog` filters are covered by unit and CLI tests, including prefix-collision and unknown-ID error paths.

### Milestone 15 — Explicit Storage Management & Garbage Collection

**Status: Planned**

- **Commands:**
  - `git2 storage` (read-only breakdown of `.git/git2/` footprint: `cas/`, `views/`, `operations/`, retained objects, reachable vs unreachable bytes)
  - `git2 gc [--dry-run] [--keep-last <N> | --keep-days <days>]`
- **Deliverables:**
  - Reachability-verified GC traversal starting from `.git/git2/HEAD` (retaining live history + both `before` and `after` views + every referenced CAS payload and retained Git object, plus any active `pending/` roots).
  - Strict dry-run preview and refusal if `.git/git2/pending/` is non-empty or a foreign/invalid state is detected (ADR D014: never prunes silently).
- **Exit criteria:**
  - Fault-injection and corruption tests prove GC never deletes any payload reachable from retained operations or active pending transactions.
  - `git2 storage` reports accurate before/after byte savings under the advisory lock.

### Milestone 16 — Streaming Scale & Concurrency Hardening

**Status: Planned**

- **Deliverables:**
  - Streaming zlib compression/decompression and incremental SHA-256 hashing for `cas/` reads and writes so capture and restore do not buffer full multi-hundred-MiB blobs in RAM.
  - Stat-cached incremental worktree scanning (skipping re-hashing unchanged files while re-verifying bytes on hash/size mismatch or config opt-in).
  - Multi-phase consistency checks comparing captured `HEAD`, refs, index, and file metadata before and after capture/plan preparation, refusing or cleanly retrying when concurrent ordinary `git` activity races an active `git2` command.
- **Exit criteria:**
  - Memory usage stays bounded when capturing/restoring files near the `policy` limit (e.g., 512 MiB+).
  - Stress tests running concurrent ordinary `git` commands alongside `git2` never silently capture torn or partial state.

---

## v0.4 — Advanced Git Workflows & Repository Formats

Expand `git2` beyond single-worktree SHA-1 repositories and introduce native/recovery-wrapped multi-step history workflows.

### Milestone 17 — Conflict-Safe Multi-Step History Operations

**Status: Planned**

- **Commands:**
  - `git2 merge` (`--ff-only`, `--no-ff`, `--abort`, `--continue`)
  - `git2 rebase` (onto target, `--continue`, `--abort`)
  - `git2 cherry-pick` / `git2 revert` (`--continue`, `--abort`)
- **Deliverables:**
  - Extend the `pending/delegated` and conflict-rollback model so mid-conflict merges, rebases, cherry-picks, and reverts can always either finish cleanly via `git2 recover` / `--continue` (stripping `REUC` cleanly) or revert atomically back to pre-operation `HEAD`, refs, index, and worktree via `git2 recover --rollback`.
- **Exit criteria:**
  - Full conflict, resolution, abort/rollback, and mid-operation crash matrices pass against ordinary Git (`git fsck --full --strict`).

### Milestone 18 — Expanded Repository Support

**Status: Future**

- **Deliverables:**
  - **Linked worktrees:** common-dir discovery, `.git` file support, shared-ref locking vs per-worktree `HEAD`/index/worktree recovery scoping.
  - **SHA-256 object format (`extensions.objectFormat = sha256`):** parameterize `GitOid` over SHA-1 (20B) and SHA-256 (32B), commit/tree parsing, loose/packed stores, and view retention maps (versioned recovery-format bump if required).
  - **Submodules & sparse/partial clones:** explicit submodule/gitlink state policy (capture pinned gitlink + nested status or coordinated nested sidecars) and safe sparse-checkout / promisor refusal-vs-opt-in boundaries.
- **Exit criteria:**
  - Each newly unlocked repository capability ships with its own restore round-trip, crash-recovery, and privacy proofs—never merely relaxing a discovery refusal.

### Milestone 19 — Hooks, Signing & Reflog Integration

**Status: Future**

- **Deliverables:**
  - Explicit opt-in policy for Git client hooks (`pre-commit`, `commit-msg`, `post-commit`, etc.) with pre-hook snapshot guarantees so failing/mutating hooks remain safely recoverable.
  - GPG and SSH commit/tag signing (`commit.gpgSign`, `tag.gpgSign`, `gpg.format`).
  - Optional Git reflog writes synchronized with native `commit`, `reset`, `branch`, and `switch` transactions.
- **Exit criteria:**
  - Opt-in hooks and signed commits/tags match ordinary Git output and `git verify-commit` / `git verify-tag` checks.

---

## v0.5 — Cross-Platform & Public Distribution

Bring verified recovery guarantees to macOS and Windows and prepare reproducible multi-platform distribution channels.

### Milestone 20 — Native macOS & Windows Recovery

**Status: Future**

- **Deliverables:**
  - **macOS (APFS / HFS+):** case-insensitive path collision detection during capture/restore, Unicode normalization (NFD vs NFC) alignment with Git's `core.precomposeUnicode`, directory `fsync`/`F_FULLFSYNC` durability review, and native macOS GitHub Actions CI.
  - **Windows (NTFS):** platform storage/locking/path layer (replacing Unix-only file descriptors/permissions with handle-based safe opens, ACL/owner checks, case-folding collision guards, symlink privilege/fallback handling, executable bit via index mode, and native Windows GitHub Actions CI).
- **Exit criteria:**
  - Full test and crash-recovery suites pass natively on Linux, macOS, and Windows runners with documented platform-specific semantics.

### Milestone 21 — Release Engineering & Packaging

**Status: Future**

- **Deliverables:**
  - Automated tag-triggered release workflow producing reproducible binaries/archives (`tar.gz` / `zip`) + SHA-256 checksums across Linux, macOS, and Windows.
  - Package manager distribution readiness (Homebrew tap/formula, `cargo-binstall` metadata, and crates.io publication readiness for `git2-cli` once explicitly authorized).
  - Clean user migration/upgrade guide between pre-1.0 recovery format versions (if format `4+` is introduced).
- **Exit criteria:**
  - Dry-run release tagging verifies checksums, provenance, and clean installation on every supported OS.

---

## v1.0 — Production-Hardened Release

### Milestone 22 — Durability, Stability & 1.0 Gate

**Status: Future**

- **Deliverables:**
  - **Power-loss durability testing:** automated filesystem fault-injection / crash-loop suite (e.g., `dm-flakey` / block-layer write-order testing on Linux) verifying durability across `cas/`, `views/`, `operations/`, `pending/`, and `.git/`.
  - **Fuzzing & property testing:** continuous fuzz targets (`cargo-fuzz` / property tests) over all binary codecs (`View`, `Operation`, `Policy`, `PendingRecord`, `DelegatedRecord`, `MutationPlan`, Git objects/indexes) and transaction recovery invariants.
  - **Format & CLI stability guarantee:** freeze recovery-store schema stability rules, backward-compatibility/migration commitments, and CLI stability guarantees.
  - **Native transport evaluation:** evaluate replacing or complementing delegated `git` remote transport (`fetch`/`push`/`clone`) with native protocol support where safety or UX justifies it.
  - **Graduate from experimental status:** remove the experimental/"not safe for important repositories" warning only after every v1.0 verification gate passes with zero known open data-loss bugs.

---

## Cross-cutting standards for every milestone

### Verification checklist

- Unit tests for binary encodings, bounds, and state transitions.
- Differential checks using ordinary Git (`git status --porcelain=v1 -z`, `git ls-files --stage -z`, `git diff --cached --raw -z`, `git cat-file`, `git rev-parse`, `git fsck --full --strict`).
- Refusal-before-mutation and corruption/truncation/trailing-byte fixtures.
- Byte-safe Unix path fixtures (non-UTF-8 bytes, spaces, tabs, newlines, leading `-`, symlinks, executables) and case-collision fixtures on macOS/Windows.
- Deterministic failpoints and child-process `abort()` crash-recovery tests for every new mutating path.
- Privacy verification proving recovery metadata never leaves `.git/git2/` or leaks over `git push --mirror`.

### Review questions

- Can this command lose state after process interruption or power loss?
- Does it capture everything it may overwrite before touching Git or worktree state?
- Can an ordinary `git` process race it silently?
- Can any path escape the repository or follow an unsafe symlink?
- Does it preserve ignored and out-of-scope state untouched?
- Does recovery metadata remain strictly local and private?
- Is failure behavior explicit and honest when a supported limit is exceeded?

### Definition of done

1. Supported behavior, limitations, and error cases are explicit in CLI output and docs.
2. Core logic lives in `git2-core` cleanly separated from `git2-cli` presentation.
3. Differential, refusal, corruption, and crash-recovery tests exist and pass with no ignored (`#[ignore]`) placeholders.
4. `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace --all-targets`, and `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` pass cleanly.
5. `README.md`, `docs/cli.md`, `docs/compatibility.md`, `docs/recovery-format.md`, `docs/decisions.md`, `docs/development.md`, `docs/runbook.md`, and `CHANGELOG.md` match the implementation with zero broken links.
