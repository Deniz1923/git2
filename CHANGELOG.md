# Changelog

All notable project changes are recorded here. The project has not made a supported release; entries are grouped under Unreleased.

## Unreleased

### Added

- Two-crate `git2-core` and `git2-cli` workspace.
- Byte-safe SHA-1 object identifiers and canonical object hashing.
- Loose-object verified reads and atomic writes.
- Packed-object reads through `gix`.
- Byte-safe commit and tree parsing/serialization.
- Private SHA-256 recovery CAS, views, operations, and advisory lock.
- Supported-repository validation and adoption baseline capture.
- External Git change import and immutable operation log.
- Two-column status with NUL-delimited byte-safe mode.
- Differential object/status tests and recovery refusal tests.
- Descriptor-relative capture of worktree bytes and executable mode, without running configured content filters during adoption.
- Format 3 recovery storage with zlib-compressed CAS payloads, persisted capture limits, owner-private access, and bounded record encoding and reads.
- `git2 checkpoint`, and private retention of Git objects needed by new captured views.
- Internal 4-phase pending-transaction journal (`Prepared`, `Applying`, `Finalizing`, `Committed`), 11 checked primitive worktree/metadata actions, staged-inode Git lock attribution, idempotent startup reconciliation/rollback, and real child process-kill crash tests.
- `git2 restore --view <id> [--dry-run]`, `git2 recover [--status | --rollback]`, pending-aware full-64-hex `git2 oplog`, dual current/target ignore-profile planning, pruned Git object rehydration, file/directory and ref D/F swap support, and core/CLI restore crash-recovery test suites.
- Native `git2 reset [--soft | --mixed | --hard]` (resolving `HEAD`, local branches, full 40-hex commit IDs, and bounded `~N` up to 10,000 steps) and `git2 undo [--dry-run]` / redo with semantic index comparison across stat-cache refreshes, plus full core and CLI crash-recovery tests.
- Native `git2 add [--all] [--] [<path>...]` literal CWD-relative staging (new/modified/deleted files, recursive directories, binary/executable/symlink fidelity, built-in `gix` text/EOL conversion, active external-filter and ignored-path refusal, transactional index lockfile updates, and differential + CLI crash tests).
- Product, architecture, compatibility, CLI, format, roadmap, testing, development, contribution, and security documentation.
- GitHub Actions CI workflow running formatting, clippy, workspace tests, and rustdoc checks.

### Changed

- Hardened worktree/Git metadata ownership checks (`core.worktree` mismatch refusal), `ObjectStore::read`/`write` symlink and verification checks, bounded object/filter reads, and D/F capture edge cases.
- Avoided redundant CAS decompressions and `fsync`s during unchanged `status`/`oplog`/`checkpoint` checks, swapped `fs2` for stdlib `File::try_lock`/`unlock`, removed unused helpers, and aligned status type-change reporting (`T`), clean `git2 add` no-ops, `git2 reset` symbolic branch refs and `--hard` EOL checkout, CLI pending guards on `status`/`checkpoint`, and repository documentation.
- Pinned the workspace to Rust 1.98.0.
- Reduced the original speculative multi-crate scaffold to one core and one CLI crate.
- Replaced placeholder commands and ignored tests with implemented vertical slices.
- Narrowed the declared target to Linux, SHA-1, non-bare, single-worktree repositories.
- Refuse shallow, split-index, alternate-object, reftable, separate metadata, and other unsupported repository layouts before recording recovery data.
- Format 1 and format 2 sidecars are refused and left untouched; there is no automatic migration.

### Removed

- Private development transcripts and obsolete planning artifacts.
- Placeholder clone, pull, push, init, commit, log, and status modules.
- Speculative backend and workflow crates.
- Write mode from `git2 debug hash-object`; it now only computes the ID.

### Known limitations

- No native `commit` or `log` CLI command yet.
- No networking or cross-platform recovery support.
- Still experimental and not recommended for important repositories.
