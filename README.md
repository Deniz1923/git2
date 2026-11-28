# git2

`git2` is an experimental Git-compatible command-line tool built around one idea: local version-control operations should be recoverable without replacing Git or publishing recovery metadata.

The project uses ordinary Git repositories as its interchange format. It adds a private, clone-local operation history under `.git/git2/` and is being developed as a learning-first, portfolio-grade implementation of Git’s local mechanics.

> [!WARNING]
> git2 is experimental (`0.1.0-alpha.1`, `publish = false`) and **not safe for important repositories**. It completes `v0.1` plus Milestone 11 (`v0.2` in progress: `adopt`, `checkpoint`, `status`, `oplog`, `restore`, `recover`, `reset`, `undo`, `add`, `commit`, `log`, `branch`, `switch`, `tag`, `remote`, `clone`, `fetch`, `pull`, and `push`); always evaluate in disposable clones and keep external backups.

## Current status

| Area | Status | Notes |
| --- | --- | --- |
| Git object interoperability | Implemented | Byte-safe SHA-1 IDs, loose reads/writes, packed reads, commits, trees, and annotated tags |
| Repository adoption | Implemented | Captures HEAD, refs, index, tracked files, and non-ignored untracked files |
| Object retention and checkpoints | Implemented | Adoption, automatic imports, and `checkpoint` retain the current Git object closure privately |
| Recovery storage | Implemented | Immutable SHA-256 zlib-compressed CAS (format 3), views, operations, integrity checks, and advisory lock |
| External Git import | Implemented | `status` and `oplog` record changed state as `external-git` operations |
| Status | Implemented | Two-column porcelain output matching Git (`git2 status -z` is byte-safe) |
| Transaction journal & restore | Implemented | 4-phase pending journal, `restore --view [--dry-run]`, and `recover [--status \| --rollback]` |
| Reset and undo | Implemented | `reset [--soft \| --mixed \| --hard]`, semantic-index `undo [--dry-run]` / redo + crash tests |
| Staging | Implemented | `add [--all] [--] [<path>...]` with built-in EOL conversion, ignore rules, and atomic index lock |
| Commits and log | Implemented | `commit -m <message>` via transactional tree/commit/ref writes (`undo`-capable) and bounded `log [-n <n>]` |
| Branches, switching, and tags | Implemented | `branch [-v \| -d \| -D \| -m]`, `switch [-c \| -C \| --detach]` with dirty-worktree overwrite protection, and `tag [-l \| -a -m \| -d]` (`undo`-capable) |
| Release readiness & CI | Implemented | Mirror-push privacy test, `PATH=""` smoke test, `scripts/demo.sh`, `scripts/package-release.sh`, [runbook](docs/runbook.md), and [verification baseline](docs/development.md#verification-baseline-010-alpha1) |
| Networking & remotes | Implemented | `clone`, `fetch`, `pull` (`--ff-only` default + conflict-recoverable `--merge`/`--rebase`), `push`, and `remote` via strict `git` argv delegation (local commands keep zero `git` runtime dependency) |

See the [full roadmap](docs/roadmap.md) (`v0.1` through `v1.0`) for completed work, upcoming milestones, and exit criteria.

## Why this project exists

Git already has reflogs and recoverable objects, but recovery is fragmented across refs, the index, worktree files, untracked files, and command-specific knowledge. git2’s intended product contract is simpler:

1. Before a supported mutation, capture the complete supported local state.
2. Apply the mutation using Git-compatible objects, refs, and index data.
3. Record the before/after transition in an immutable local operation log.
4. Let the user restore an earlier view without rewriting or deleting that history.

The current build implements all of `v0.1` plus `v0.2` Milestone 11: private format-3 storage, external-change import, the 4-phase pending-transaction journal, verified `restore --view`, `recover`, `reset`, `undo`/redo, `add`, `commit -m`, bounded `log`, `branch`, `switch`, `tag`, plus delegated `remote`, `clone`, `fetch`, `pull`, and `push`.

## Supported environment

The active target is an ordinary, non-bare, single-worktree SHA-1 repository on Linux. git2 refuses known states it cannot restore safely, including linked worktrees, sparse indexes, partial clones, submodules, unresolved conflicts, and in-progress merge/rebase-like operations.

Ignored files and repository-local Git configuration are outside the recovery guarantee. The default eligible regular-file limit is 100 MiB; `adopt --max-file-mib` accepts up to 1024 MiB and persists the choice for future imports. New sidecars use format 3 with owner-private permissions and zlib-compressed CAS records. Format 1 and format 2 stores are refused and left unchanged; see [recovery format](docs/recovery-format.md) before starting a new store.

Object retention has its own bounds (128 MiB per Git object payload, 1,000,000 distinct retained objects, pack delta depth 64, and the whole-view limit) and refuses indexes with resolve-undo (REUC) entries and historical gitlink trees. Read the [compatibility matrix](docs/compatibility.md) before experimenting.

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

`checkpoint` explicitly saves the current state of an adopted repository and prints the full 64-hex recovery view ID. A changed state appends one `git2:checkpoint` operation; an unchanged state returns the same view and appends nothing. git2 does not monitor ordinary Git in the background, so a checkpoint captures the state at the moment you run it. Captures retain the Git objects currently reachable from the saved refs, HEAD, and index, and saved views can be previewed with `git2 restore --view <id> --dry-run` and applied with `git2 restore --view <id>`.

```sh
printf 'checked point\n' > hello.txt
/path/to/git2 checkpoint
/path/to/git2 oplog
/path/to/git2 restore --view <64-hex-view-id> --dry-run
```

Or run the reproducible disposable-repository walkthrough script directly:

```sh
bash scripts/demo.sh
```

## Implemented commands

```text
git2 adopt [--max-file-mib <MiB>]
git2 checkpoint
git2 add [--all] [--] [<path>...]
git2 commit -m <message>
git2 log [-n|--max-count <n>]
git2 branch [-v] | <name> [<start-point>] | -d|-D <name> | -m [<old>] <new>
git2 switch [-c|-C <new-branch>] [-d|--detach] [<target>]
git2 tag [-l] | <name> [<target>] | -a -m <message> <name> [<target>] | -d <name>
git2 reset [--soft | --mixed | --hard] [<commit>]
git2 undo [--dry-run]
git2 restore --view <64-hex-view-id> [--dry-run]
git2 recover [--status | --rollback]
git2 status [-z|--nul]
git2 oplog
git2 remote [-v] | add | remove | rename | get-url | set-url
git2 clone [--branch <name>] [--origin <name>] [--no-adopt] [--max-file-mib <MiB>] [--] <url> [<dir>]
git2 fetch [--all] [-p|--prune] [-t|--tags] [--] [<remote>] [<refspec>...]
git2 pull [--ff-only | --merge | --rebase] [-p|--prune] [--] [<remote>] [<refspec>...]
git2 push [-u|--set-upstream] [--tags] [-d|--delete] [--force-with-lease | -f|--force] [-n|--dry-run] [--] [<remote>] [<refspec>...]
git2 debug hash-object [--stdin | <path>]
git2 debug cat-object [-t|--type-only] <full-sha1>
```

The debug commands expose the object layer for interoperability work; they are read-only and are not the intended long-term user interface. See the [CLI reference](docs/cli.md) for exact behavior.

## Architecture

The workspace deliberately contains two crates:

| Crate | Responsibility |
| --- | --- |
| `git2-core` | Git object model, repository validation, recovery store, snapshots, object retention, operation history, and status |
| `git2-cli` | Argument parsing, output, and command orchestration |

`git2` owns its local semantics (with zero runtime `git` binary dependency for local operations). `gix` supplies difficult plumbing such as packed-object reads, index parsing, ignores/path traversal, and status machinery. Network and remote commands (`clone`, `fetch`, `pull`, `push`, `remote`) delegate transport and credentials to the installed `git` executable via strict argument vectors (`-c protocol.ext.allow=never`) while `git2` owns recovery snapshots, `pending/delegated` conflict rollback, `oplog`, and privacy guarantees.

Read the [architecture](docs/design.md) and [recovery format](docs/recovery-format.md) for details.

## Documentation

- [Architecture and invariants](docs/design.md)
- [Compatibility and limitations](docs/compatibility.md)
- [CLI reference](docs/cli.md)
- [Recovery format](docs/recovery-format.md)
- [Operations & recovery runbook](docs/runbook.md)
- [Roadmap (`v0.1`–`v1.0`)](docs/roadmap.md)
- [Development, testing & verification](docs/development.md)
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
