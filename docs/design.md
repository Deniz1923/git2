# Architecture and invariants

## Purpose

git2 is a Git-compatible local version-control client whose differentiator is non-destructive operation-level recovery. It is not a new repository format, hosting service, or attempt to replace the Git ecosystem.

The design has two compatibility obligations:

- Git must continue to understand every ordinary object, ref, and index write made by git2.
- git2 recovery metadata must stay private to one clone and must never become a custom Git ref that an ordinary push can publish.

## System shape

```text
                         ordinary Git tools
                                │
                                ▼
worktree ───── index ───── refs/HEAD ───── Git object database
    │             │             │                    │
    └─────────────┴─────────────┴────────────────────┘
                                │ capture / compare
                                ▼
                       git2 local repository core
                         │       │        │
                         │       │        └── status model
                         │       └── Git object codecs/store
                         └── recovery engine
                                  │
                                  ▼
                             .git/git2/
                      CAS + views + operations
```

The ordinary Git repository remains authoritative for collaboration and interoperability. The sidecar records local views and transitions; it is not consulted by Git and is not necessary to clone or push the repository.

## Workspace boundaries

### `git2-core`

The core currently owns:

- SHA-1 object identifiers and canonical Git object hashing;
- byte-safe raw objects, signatures, commits, trees, and tree modes;
- atomic loose-object writes and verified loose-object reads;
- packed-object reads delegated to `gix`;
- supported-repository discovery and refusal checks;
- snapshot capture of HEAD, refs, index, and eligible worktree entries;
- SHA-256 zlib-compressed content-addressed recovery storage (format 3);
- Git object-closure retention and explicit `checkpoint` capture;
- immutable views and operation records;
- advisory locking and external-change import;
- deterministic two-column status modeling.

The core is private to this workspace before 1.0. Rust APIs may change freely; Git writes and documented recovery-format compatibility require explicit review.

### `git2-cli`

The CLI owns:

- command-line parsing;
- human and porcelain output;
- error context;
- orchestration such as importing external changes before showing status or operation history.

Business rules and on-disk encodings do not belong in the CLI.

### When to add another crate

The workspace stays at two crates until a component has an independently useful boundary, a distinct dependency profile, or a separately testable security boundary. File count alone is not a reason to split a crate.

## Dependency policy

git2 implements the semantics that teach and distinguish the project, while reusing narrow plumbing where a second implementation would add risk without product value.

| Area | Owner |
| --- | --- |
| Loose object canonicalization and writes | git2 |
| Commit/tree byte encoding | git2 |
| Recovery format and operation semantics | git2 |
| Product-level safety/refusal policy | git2 |
| Packed object lookup | `gix` |
| Index decoding and future transactional index writes | `gix` |
| Ignore/path traversal and status plumbing | `gix` |
| SHA-1 and SHA-256 compression/digests | focused Rust crates |
| Future clone/fetch/push transport | installed `git` subprocess initially |

Subprocesses must use argument vectors, never shell interpolation. Networking is deferred until the local recovery contract is proven.

## Data domains

### Git domain

Git object IDs are SHA-1 for the current target. Object bytes follow Git’s canonical format:

```text
<kind> <decimal-length> NUL <payload>
```

Commit messages, identities, header values, and paths are bytes, not assumed UTF-8. Tree entries retain order when parsed and serialized. Gitlinks can be represented by the object model but are rejected by repository adoption because submodule state is not recoverable yet.

### Recovery domain

Recovery content, views, and operations use SHA-256 identifiers. This intentionally separates private recovery identity from Git object identity.

A view captures:

- raw `.git/HEAD` bytes;
- loose ref files below `.git/refs/`;
- raw `.git/packed-refs`, if present;
- raw index bytes, if an index exists;
- present tracked worktree entries;
- present non-ignored untracked worktree entries;
- regular/executable/symlink kind and content for each captured path.

Ignored files, Git config, hooks, reflogs, object packs, remotes, and platform metadata such as ownership are not part of a view.

An operation links a parent operation and before/after view IDs with a kind and Unix timestamp. Existing operation and view records are immutable.

## Implemented read/capture flows

### Adoption

```text
discover repository
  → reject unsupported state
  → enumerate eligible state
  → reject oversized/special files
  → initialize .git/git2
  → acquire recovery lock
  → write deduplicated CAS content
  → write immutable baseline view
  → write immutable adopt operation
  → atomically select operation with git2 HEAD
```

The file-size and repository-feature checks occur before sidecar creation where practical. A failure during persistence may leave unreachable immutable CAS data, but HEAD is updated only after the selected view and operation exist.

### External-change import

```text
open existing recovery store
  → acquire exclusive advisory lock
  → read current operation/view
  → capture current Git/worktree state
  → persist deduplicated content and view
  → if view unchanged: append nothing
  → otherwise append external-git operation
  → atomically update git2 HEAD
```

External import is currently triggered by `git2 status` for adopted repositories and by `git2 oplog`; `git2 checkpoint` explicitly saves the current state as `git2:checkpoint`. None of these commands run a background watcher.

### Status

Status uses `gix` to compare HEAD → index and index → worktree. git2 merges both sides into a deterministic path-sorted model and emits two-column porcelain. `-z` emits raw path bytes with NUL record delimiters.

## Implemented mutation protocol

Implemented Git mutations (`restore`, `reset`, `undo`, `add`, `commit`, and recovery via `recover`) run through a constrained 4-phase (`Prepared -> Applying -> Finalizing -> Committed`) state machine over a fixed vocabulary of primitive checked actions:

1. Validate all repository and command preconditions.
2. Acquire the git2 recovery lock.
3. Import any state change made by another Git client.
4. Persist and fsync the complete pre-operation view.
5. Write and fsync a pending transaction record (`Prepared`).
6. Advance through `Applying` using individually verifiable primitive actions (object rehydration, checked worktree writes/removals, empty-directory updates, index writes/removals, and transactional ref/HEAD updates).
7. Capture and verify the post-operation view (`Finalizing`), append the immutable git2 operation, and atomically update git2 HEAD (`Committed`).
8. Remove the pending record and sync its directory.

On startup, `git2 recover` reconciles a pending record idempotently before another mutation: aborting cleanly if `Prepared`, resuming and finalizing if `Applying`/`Finalizing`, or cleaning pending state without duplicating history if recovery `HEAD` already selected the deterministic operation ID. (`--rollback` can also revert un-selected `Prepared`/`Applying`/`Finalizing` states.)

## Undo semantics

Undo is a new operation, not history deletion:

```text
operation N: before=A, after=B, kind=git2:reset
operation N+1: before=B, after=A, kind=git2:undo
```

Restoring a view updates HEAD/refs/index/worktree as a checked transaction. It first snapshots the current state, avoids following unsafe paths or symlinks, removes eligible paths absent from the target view, restores exact captured bytes and supported modes, and appends its own operation. `git2 undo` ignores stat-cache-only index refreshes (such as plain `git status`) while still requiring exact staged paths, stages, modes, OIDs, and flags, and still restoring exact captured raw index bytes. Redo can then undo the undo operation.

## Safety invariants

The following invariants are already enforced:

- Recovery IDs are recomputed and checked when records are read.
- Existing immutable records are never silently overwritten with different bytes.
- Writes use create-new temporary files, file and directory sync, no-replacement publication for immutable data, and rename for the mutable pointer.
- A selected operation and its referenced records must exist before git2 HEAD can point to it.
- Only one git2 writer can hold the advisory lock.
- Recovery metadata is below `.git/git2/`, never `refs/`.
- Paths are byte-safe on Unix and repository-relative.
- Empty, absolute, `.`, `..`, empty-component, and `.git` path components are rejected during worktree path construction.
- Known unsupported repository states are refused.
- Oversized eligible regular files block adoption/import.

The following invariants are required before the first mutation:

- A durable pending record exists before changing Git state.
- Recovery from interruption is idempotent and tested at each write boundary.
- Restore never writes through a symlink or outside the worktree.
- Ref and index writes use lockfile/compare-and-swap semantics.
- The pre-operation view remains reachable even if post-operation capture fails.

## Concurrency model

The current advisory lock serializes cooperating git2 processes. Ordinary Git does not take this lock, so Git may still change repository state during a capture. Current capture errors are surfaced, but full concurrent-change detection is not yet proven.

Before mutating commands ship, captures need a consistency check across HEAD, refs, index, and file metadata, with retry or refusal if another process changes them during the transaction.

## Error policy

The default is refusal, not best effort. Unknown formats, corrupt content, unsupported features, unsafe paths, excessive files, lock contention, and malformed objects return errors. Before 1.0, recovery-format migration is not automatic; an unsupported version is refused.

## Performance and storage

Snapshots are content-addressed, so unchanged file and index bytes deduplicate naturally. Views and operations are small immutable records. There is no garbage collector or silent pruning policy yet.

The CLI warns when an adoption captures at least 5 GiB. The default limit is 100 MiB per eligible regular file. `--max-file-mib` sets a persisted limit of up to 1024 MiB. Keeping per-file bounds tight prevents unbounded memory use during in-memory captures and restores.

Future performance work should follow correctness evidence: streaming CAS writes, bounded memory, incremental scans, and explicit `git2 gc` reporting are later milestones.
