# Development guide

## Prerequisites

- Rust 1.98.0 through `rustup`;
- Git on `PATH`;
- Linux for full repository/recovery development;
- enough free disk space for `gix` and debug artifacts.

The pinned toolchain includes `rustfmt` and `clippy`:

```sh
rustup show
cargo --version
git --version
```

## Workspace

```text
.
├── Cargo.toml
├── .github/workflows/ci.yml
├── crates/
│   ├── git2-core/
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── commit.rs
│   │   │   ├── log.rs
│   │   │   ├── object.rs
│   │   │   ├── paths.rs
│   │   │   ├── recovery.rs
│   │   │   ├── recovery/disk.rs
│   │   │   ├── repository.rs
│   │   │   ├── reset.rs
│   │   │   ├── restore.rs
│   │   │   ├── retention.rs
│   │   │   ├── stage.rs
│   │   │   ├── status.rs
│   │   │   ├── store.rs
│   │   │   ├── store/packed.rs
│   │   │   ├── transaction.rs
│   │   │   └── undo.rs
│   │   └── tests/
│   └── git2-cli/
│       ├── src/main.rs
│       └── tests/
├── docs/
└── scripts/
    ├── demo.sh
    └── package-release.sh
```

Module responsibilities:

- `commit`: native `commit -m` tree/commit/ref construction over the 4-phase pending journal;
- `log`: bounded first-parent history traversal from `HEAD`;
- `object`: byte-safe Git object model and codecs;
- `paths`: descriptor-relative, no-follow worktree & metadata path operations;
- `recovery` / `recovery::disk`: owner-private SHA-256 zlib CAS store and binary records;
- `repository`: discovery, refusal policy, capture, and external import;
- `retention`: reachable Git object closure verification and retention;
- `transaction`: 4-phase pending journal and crash recovery;
- `restore`: verified `restore --view` planning and execution;
- `reset`: native `reset [--soft | --mixed | --hard]` over commits and branches;
- `undo`: semantic-index `undo` / redo over completed operations;
- `stage`: native `add` literal staging with built-in EOL conversion;
- `status`: merged HEAD/index/worktree status model;
- `store` / `store::packed`: loose object I/O plus bounded packed-object reads;
- CLI `main`: parsing, output, and orchestration only.

## Common commands

Fast iteration:

```sh
cargo check --workspace --all-targets
cargo test -p git2-core
cargo test -p git2-cli
```

Full gate:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Run the CLI:

```sh
cargo run -p git2-cli -- --help
cargo run -p git2-cli -- status -z
```

Demo and local release packaging:

```sh
bash scripts/demo.sh
bash scripts/package-release.sh
```

Generated build output can be large. `cargo clean` removes `target/`; it does not touch source or repository fixtures.

## Development workflow

1. Choose one vertical slice from the roadmap.
2. Write its supported/refused behavior and exit criteria first.
3. Add a minimal unit or differential fixture that expresses the behavior.
4. Implement core behavior without CLI presentation coupling.
5. Add the CLI only after the core result is testable.
6. Add corruption/refusal cases.
7. For mutations, add failpoints and restart tests in the same slice.
8. Run the full gate.
9. Update README, CLI reference, compatibility, roadmap, decisions, recovery format, and changelog as applicable.

Do not scaffold later commands with `todo!`. An absent command is more honest than a command that parses and panics.

## Rust conventions

- Unsafe Rust is forbidden by workspace lint.
- Clippy `all` and `pedantic` warnings are denied in the gate.
- Public core items require rustdoc, including `# Errors` for fallible APIs.
- Prefer typed IDs and enums over bare strings/integers.
- Preserve error sources where practical; add human context at the CLI boundary.
- Keep on-disk parsing bounded and reject trailing/unknown data.
- Do not expose private core APIs as stable before 1.0.

## Byte and path rules

Git names, commit data, and Unix paths are not inherently UTF-8.

- Use `BString`/`BStr` or byte slices for repository-relative paths and Git payloads.
- Use `Path`/`OsStr` only at the filesystem boundary.
- Never call `to_string_lossy` to make a correctness decision.
- Human diagnostics may use lossy/debug rendering, but machine output must preserve bytes.
- Use NUL-delimited interfaces for differential tests involving arbitrary paths.
- Validate every path before using captured data to access a worktree.

## Filesystem mutation rules

New writes must follow the project’s durability pattern:

- use explicit validated targets;
- create parent directories narrowly;
- write a create-new temporary file in the target directory;
- write all bytes and sync the file;
- rename atomically;
- sync the parent directory according to the documented platform policy;
- clean up temporary files after recoverable errors;
- verify immutable destinations if another writer created them first.

Never overwrite Git refs, index, or worktree state from a user-facing command outside the transaction runner planned in Milestone 4.

## Git compatibility rules

- Treat Git as the oracle for supported semantics.
- After object/ref/index changes, verify with ordinary Git and `git fsck`.
- Respect lockfile and compare-and-swap behavior; do not write refs or index with plain truncating file writes.
- Do not create `refs/git2/*` or another public recovery ref namespace.
- Do not shell-interpolate Git subprocess arguments.
- Keep networking out of the core local transaction until v0.2.

## Recovery-format changes

The recovery format is product state, even before the Rust API is stable.

Before changing it:

1. Read `docs/recovery-format.md` in full.
2. Decide whether the change is compatible.
3. Update magic/version if incompatible.
4. Keep readers bounded.
5. Add old/new binary fixtures and corruption tests.
6. Update reachability and pending-transaction reasoning.
7. Add a changelog and decision entry.

Do not silently reinterpret existing bytes.

## Dependencies

Add a dependency only when it owns plumbing outside git2’s learning/product boundary or materially reduces safety risk.

For each dependency change:

- explain why the standard library/current dependency set is insufficient;
- disable unnecessary default features where practical;
- inspect the resulting feature graph and lockfile;
- consider license and minimum Rust version;
- keep network/protocol dependencies out of local-only milestones.

## Documentation rules

Every claim must be labeled by reality:

- implemented now;
- required/design target;
- planned/deferred.

Avoid future tense that sounds like a guarantee. Commands that do not exist belong in the roadmap. Recovery docs must never imply that captured data is restorable until the restore gate passes.

Use relative links inside repository Markdown. Run a link/file check when adding or renaming docs.

## Review checklist

### Correctness

- Are Git’s byte formats and modes represented exactly?
- Is behavior compared with Git?
- Are packed and loose objects both considered?
- Are errors classified without fragile string matching where an API exists?

### Safety

- Is every overwritten state captured first?
- Is the capture reachable durably before mutation?
- What happens after interruption at each boundary?
- Can paths escape or traverse symlinks?
- Can ordinary Git race the operation?
- Are unsupported states refused before writes?

### Product honesty

- Does the CLI expose only working behavior?
- Does documentation name limitations?
- Is the compatibility matrix updated?
- Is a safety claim backed by an automated test?

## Release process target

The first experimental release process will be finalized in Milestone 9. At minimum it must:

1. run all quality and failure gates on a clean Linux runner;
2. build in release mode;
3. run the documented disposable demo;
4. update version and changelog;
5. produce a binary archive and SHA-256 checksum;
6. publish explicit limitations and recovery-format version;
7. retain the “experimental / not for important repositories” warning.
