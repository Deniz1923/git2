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
- SHA-256 content-addressed recovery storage;
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

External import is currently triggered by `git2 status` for adopted repositories and by `git2 oplog`. It is not a background watcher.

### Status

Status uses `gix` to compare HEAD → index and index → worktree. git2 merges both sides into a deterministic path-sorted model and emits two-column porcelain. `-z` emits raw path bytes with NUL record delimiters.

## Target mutation protocol

No user-facing Git mutation is implemented yet. Every future mutation must use this protocol:

1. Validate all repository and command preconditions.
2. Acquire the git2 recovery lock.
3. Import any state change made by another Git client.
4. Persist and fsync the complete pre-operation view.
5. Write and fsync a pending transaction record.
6. Apply Git changes atomically where Git supports it.
7. Capture and persist the post-operation view.
8. Append the immutable git2 operation.
9. Atomically update git2 HEAD.
10. Remove the pending record and sync its directory.

On startup, a pending record must be reconciled before another mutation: either prove nothing changed and discard it, or capture the resulting state and finalize a recoverable operation. A mutation must never be exposed merely because its happy-path implementation works.

## Target undo semantics

Undo is a new operation, not history deletion:

```text
operation N: before=A, after=B, kind=git2:reset
operation N+1: before=B, after=A, kind=git2:undo
```

Restoring a view must update HEAD/refs/index/worktree as a checked transaction. It must first snapshot the current state, avoid following unsafe paths or symlinks, remove eligible paths absent from the target view, restore exact captured bytes and supported modes, and append its own operation. Redo can then restore the undo operation’s `before` view.

These are design requirements, not current capabilities.

## Safety invariants

The following invariants are already enforced:

- Recovery IDs are recomputed and checked when records are read.
- Existing immutable records are never silently overwritten with different bytes.
- Writes use create-new temporary files, `fsync`, and rename.
- A selected operation must exist before git2 HEAD can point to it.
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

The CLI warns when an adoption captures at least 5 GiB. The default limit is 100 MiB per eligible regular file and can be changed for adoption with `--max-file-mib`. Changing the limit expands risk and does not change the current lack of restore. The selected override is not persisted yet, so automatic imports return to the default.

Future performance work should follow correctness evidence: streaming CAS writes, bounded memory, incremental scans, and explicit `git2 gc` reporting are later milestones.
