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
- Format 2 recovery storage with persisted capture limits, owner-private access, and bounded record encoding and reads.
- `git2 checkpoint`, and private retention of Git objects needed by new captured views.
- Product, architecture, compatibility, CLI, format, roadmap, testing, development, contribution, and security documentation.

### Changed

- Pinned the workspace to Rust 1.98.0.
- Reduced the original speculative multi-crate scaffold to one core and one CLI crate.
- Replaced placeholder commands and ignored tests with implemented vertical slices.
- Narrowed the declared target to Linux, SHA-1, non-bare, single-worktree repositories.
- Refuse shallow, split-index, alternate-object, reftable, separate metadata, and other unsupported repository layouts before recording recovery data.
- Format 1 sidecars are refused and left untouched; there is no automatic migration.

### Removed

- Private development transcripts and obsolete planning artifacts.
- Placeholder clone, pull, push, init, commit, log, and status modules.
- Speculative backend and workflow crates.
- Write mode from `git2 debug hash-object`; it now only computes the ID.

### Known limitations

- No staging, commit, reset, restore, or undo command.
- No pending-transaction/crash-recovery journal for mutations.
- No networking or cross-platform recovery support.
- Captured recovery views are not yet restorable.
