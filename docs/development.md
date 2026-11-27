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
│   │   │   ├── network.rs
│   │   │   ├── object.rs
│   │   │   ├── paths.rs
│   │   │   ├── recovery.rs
│   │   │   ├── recovery/disk.rs
│   │   │   ├── remote.rs
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
- `network`: `clone`, `fetch`, `pull`, and `push` via strict `git` argv delegation, credential redaction, and `pending/delegated` transactions;
- `remote`: `remote` management (`list`, `add`, `remove`, `rename`, `get-url`, `set-url`) over strict argv delegation;
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

Never overwrite Git refs, index, or worktree state from a local mutating `git2` command outside the 4-phase pending-transaction runner (`transaction.rs`), and run delegated Git network mutations through `pending/delegated`.

## Git compatibility rules

- Treat Git as the oracle for supported semantics.
- After object/ref/index changes, verify with ordinary Git and `git fsck`.
- Respect lockfile and compare-and-swap behavior; do not write refs or index with plain truncating file writes.
- Do not create `refs/git2/*` or another public recovery ref namespace.
- Do not shell-interpolate Git subprocess arguments; use strict argument vectors (`-c protocol.ext.allow=never` and `--`).
- Keep local `git2` commands completely independent of an installed `git` binary at runtime.

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

Avoid future tense that sounds like a guarantee. Unimplemented commands belong in the [roadmap](roadmap.md). Recovery docs must make clear what is verified by tests today versus what remains planned.

Use relative links inside repository Markdown. Run a link/file check when adding or renaming docs.

## Test suite and CI

Tests exist to prove Git interoperability and recovery safety using ordinary `git` as the compatibility oracle, byte-level format checks, and child-process `std::process::abort()` crash matrices. Tests create isolated temporary repositories and require `git` on `PATH` (with `GIT_TERMINAL_PROMPT=0` for network tests; no network access or global author configuration required).

### Active integration test suites

- **Core (`crates/git2-core/tests/`):**
  - `object_interop.rs`: differential canonical blob/loose/packed reads after `git gc --prune=now`.
  - `recovery_format.rs`: format 3 binary codecs, bounds, truncation, trailing bytes, legacy format 1/2 refusal.
  - `adoption.rs` & `retention.rs`: sidecar creation, policy bounds, ignore/refusal rules, external-change import, reachable Git object closure retention.
  - `status_interop.rs`: differential `status` / `-z` parity across modes, symlinks, non-UTF-8/tab/newline paths, unborn/detached `HEAD`, nested/global ignores.
  - `transaction.rs`: 4-phase pending journal, staged-inode lock attribution, foreign-lock refusal, failpoints, real child `abort()`, double crashes, and rollback.
  - `restore.rs`: `--dry-run`, dual ignore profiles, pruned-object rehydration, file/dir & ref D/F swaps.
  - `reset_undo.rs`: differential `--soft` / `--mixed` / `--hard`, semantic-index `undo` / redo, divergence checks.
  - `stage.rs` & `commit_log.rs`: differential literal staging, built-in EOL conversion, canonical tree/commit construction, bounded `log`, hook/GPG/encoding refusals.
  - `network.rs`: `remote`, `clone` auto-adoption, undoable `fetch`, `pull` (`--ff-only` / `--merge` / `--rebase` + conflict rollback & `REUC` stripping), non-undoable `push`, and credential redaction.
- **CLI (`crates/git2-cli/tests/`):**
  - `cli.rs`, `crash_recovery.rs`, `reset_undo_cli.rs`, `stage_cli.rs`, `commit_log_cli.rs`, `network_cli.rs`: end-to-end CLI behavior plus `GIT2_TEST_FAILPOINT` child process-kill and rollback matrices.
  - `release_readiness.rs`: `git push --mirror` privacy verification and `PATH=""` zero-runtime-`git` smoke test across all local commands.

### Testing rules

- **Git as oracle:** compare durable state (`git hash-object`, `git cat-file`, `git status --porcelain=v1 -z`, `git diff --cached --raw -z`, `git ls-files --stage -z`, `git rev-parse`, `git fsck --full --strict`).
- **Assert reopened state:** reopen repositories after mutations and verify Git integrity, index validity, worktree bytes/modes, and `git2` `HEAD`/parent/view links.
- **Keep paths byte-safe:** cover invalid UTF-8, spaces, tabs, newlines, leading `-`, symlinks, and executables via NUL-delimited interfaces (`-z`).
- **Test refusal before mutation:** verify unsupported states return errors with zero disk/sidecar writes.
- **No ignored placeholders:** do not check in `#[ignore]`, `todo!()`, or `unimplemented!()` tests.

### CI workflow

`.github/workflows/ci.yml` runs on every push to `main` and pull request on `ubuntu-latest`:

1. Logs `rustc --version`, `cargo --version`, and `git --version`;
2. `cargo fmt --all -- --check`;
3. `cargo clippy --workspace --all-targets -- -D warnings`;
4. `cargo test --workspace --all-targets`;
5. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`;
6. `cargo build --release --workspace`;
7. `GIT2_BIN="$PWD/target/release/git2" bash scripts/demo.sh`.

## Verification baseline (`0.1.0-alpha.1`)

- **License audit (`cargo metadata --format-version 1`):** `163` resolved packages total; both workspace crates are `MIT` with `unsafe_code = "forbid"`; all transitive dependencies carry permissive or dual-permissive licenses (`MIT`, `Apache-2.0`, `BSD-3-Clause`, `0BSD`, `Unlicense`, `CC0-1.0`, `Zlib`, `Unicode-3.0`, plus one `MIT OR Apache-2.0 OR LGPL-2.1-or-later` crate usable under `MIT OR Apache-2.0`), with zero copyleft-only or unknown licenses.
- **Baseline performance & disk footprint (`--release`, Linux, 200 tracked ~1 KiB files across 10 dirs):**
  - `git2 adopt`: ~117.2 ms;
  - `git2 status -z`: ~33.4 ms;
  - `git2 add` (1 modified file): ~160.4 ms;
  - `git2 commit -m "edit one"`: ~134.8 ms;
  - `git2 log -n 10`: ~2.6 ms;
  - `git2 reset --hard HEAD~1`: ~154.4 ms;
  - `git2 undo`: ~201.7 ms;
  - `.git/git2/` disk footprint after that full sequence: **161,611 bytes** (`16,075` bytes in `.git/objects/`). Note that `git2` currently buffers eligible files in RAM during capture; streaming CAS is tracked in `v0.3` ([roadmap](roadmap.md)).

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

## Local release packaging

`scripts/package-release.sh` produces a versioned Linux archive (`target/release-dist/git2-<version>-linux-<arch>.tar.gz` and `.sha256`, bundling `README.md`, `LICENSE-MIT`, `SECURITY.md`, and `docs/runbook.md`). Both workspace crates currently keep `publish = false`, and public release tagging or binary distribution remains gated on separate maintainer authorization and `v0.5` release engineering ([roadmap](roadmap.md)).
