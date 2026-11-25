# git2

`git2` is an experimental Git-compatible command-line tool built around one idea: local version-control operations should be recoverable without replacing Git or publishing recovery metadata.

The project uses ordinary Git repositories as its interchange format. It adds a private, clone-local operation history under `.git/git2/` and is being developed as a learning-first, portfolio-grade implementation of Git’s local mechanics.

> [!WARNING]
> git2 is not ready for important repositories. It can capture and inspect recovery history, but restore and undo are not implemented yet. A snapshot that cannot yet be restored is not a backup.

## Current status

| Area | Status | Notes |
| --- | --- | --- |
| Git object interoperability | Implemented | Byte-safe SHA-1 IDs, loose reads/writes, packed reads, commits, and trees |
| Repository adoption | Implemented | Captures HEAD, refs, index, tracked files, and non-ignored untracked files |
| Recovery storage | Implemented | Immutable SHA-256 CAS, views, operations, integrity checks, and advisory lock |
| External Git import | Implemented | `status` and `oplog` record changed state as `external-git` operations |
| Status | Implemented | Two-column porcelain output; `-z` is byte-safe |
| Staging and commits | Not implemented | Next mutation milestone |
| Restore and undo | Not implemented | Core product proof; do not rely on recovery data yet |
| Networking | Not implemented | Deferred until the local safety model works |

See the [full roadmap](docs/roadmap.md) for milestones and exit criteria.

## Why this project exists

Git already has reflogs and recoverable objects, but recovery is fragmented across refs, the index, worktree files, untracked files, and command-specific knowledge. git2’s intended product contract is simpler:

1. Before a supported mutation, capture the complete supported local state.
2. Apply the mutation using Git-compatible objects, refs, and index data.
3. Record the before/after transition in an immutable local operation log.
4. Let the user restore an earlier view without rewriting or deleting that history.

The current build proves steps of the storage and observation model. It does not yet prove steps 2–4 end to end.

## Supported environment

The active target is an ordinary, non-bare, single-worktree SHA-1 repository on Linux. git2 refuses known states it cannot restore safely, including linked worktrees, sparse indexes, partial clones, submodules, unresolved conflicts, and in-progress merge/rebase-like operations.

Ignored files and repository-local Git configuration are outside the recovery guarantee. The default maximum eligible file size is 100 MiB. A custom adoption limit is not persisted yet; later automatic imports currently return to the 100 MiB default.

Read the [compatibility matrix](docs/compatibility.md) before experimenting.

## Build

Requirements:

- Rust 1.98.0, installed automatically by `rustup` from `rust-toolchain.toml`;
- Git on `PATH` for differential tests;
- Linux for the currently supported repository and recovery commands.

```sh
cargo build --workspace
cargo test --workspace --all-targets
```

The binary is produced at `target/debug/git2`. To install it locally from this checkout:

```sh
cargo install --path crates/git2-cli
```

## Safe experiment

Use a disposable repository:

```sh
mkdir /tmp/git2-demo
cd /tmp/git2-demo
git init
printf 'hello\n' > hello.txt
git add hello.txt
git -c user.name=Demo -c user.email=demo@example.invalid commit -m initial

/path/to/git2 adopt
printf 'changed\n' > hello.txt
/path/to/git2 status
/path/to/git2 oplog
```

`status` or `oplog` notices the change made outside git2 and appends one `external-git` operation. Repeating the command without another state change does not append duplicate history.

## Implemented commands

```text
git2 adopt [--max-file-mib <MiB>]
git2 status [-z|--nul]
git2 oplog
git2 debug hash-object [-w|--write] [--stdin | <path>]
git2 debug cat-object [-t|--type-only] <full-sha1>
```

The debug commands expose the object layer for interoperability work; they are not the intended long-term user interface. See the [CLI reference](docs/cli.md) for exact behavior.

## Architecture

The workspace deliberately contains two crates:

| Crate | Responsibility |
| --- | --- |
| `git2-core` | Git object model, repository validation, recovery store, snapshots, operation history, and status |
| `git2-cli` | Argument parsing, output, and command orchestration |

git2 owns its local semantics. `gix` supplies difficult plumbing such as packed-object reads, index parsing, ignore/path traversal, and status machinery. Future networking will initially call the installed Git executable with argument vectors rather than reimplementing the wire protocol.

Read the [architecture](docs/design.md) and [recovery format](docs/recovery-format.md) for details.

## Documentation

- [Product definition](docs/product.md)
- [Architecture and invariants](docs/design.md)
- [Compatibility and limitations](docs/compatibility.md)
- [CLI reference](docs/cli.md)
- [Recovery format](docs/recovery-format.md)
- [Roadmap](docs/roadmap.md)
- [Testing strategy](docs/testing.md)
- [Development guide](docs/development.md)
- [Architecture decisions](docs/decisions.md)
- [Contributing](CONTRIBUTING.md)
- [Security policy](SECURITY.md)
- [Changelog](CHANGELOG.md)

## Quality gates

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

Differential tests create temporary repositories and compare git2’s results with the installed `git` executable. Mutating features are not considered complete merely because unit tests pass; each roadmap milestone has explicit safety and interoperability exit criteria.

## License

MIT. See [LICENSE-MIT](LICENSE-MIT).
