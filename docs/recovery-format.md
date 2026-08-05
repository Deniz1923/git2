# Recovery format

## Status and compatibility

The recovery sidecar format is currently version 1 and is experimental. The reader refuses unknown top-level versions. Before 1.0, automatic migration is not promised; any incompatible change must increment the top-level version and update this document and fixtures.

The format is private to one clone. It is stored below the Git directory and never creates refs:

```text
.git/git2/
  format
  HEAD
  lock
  cas/<2 hex>/<62 hex>
  views/<2 hex>/<62 hex>
  operations/<2 hex>/<62 hex>
  pending/
```

`pending/` is reserved but unused. No current command mutates HEAD, refs, index, or worktree, so the crash-recovery journal has not been implemented yet.

## Design goals

- Keep recovery metadata unreachable through ordinary Git refs and pushes.
- Preserve Unix paths and Git metadata as bytes rather than requiring UTF-8.
- Deduplicate identical file and index content.
- Make content corruption detectable by name/content verification.
- Keep views and operations immutable.
- Select a new history head only after all referenced immutable records exist.
- Bound decoder allocations when reading untrusted or damaged records.
- Leave room for a durable pending-transaction protocol before mutations ship.

## Non-goals

Version 1 does not:

- encrypt recovery content;
- authenticate content against a secret key;
- compress CAS payloads;
- garbage-collect unreachable records;
- preserve ACLs, ownership, xattrs, or timestamps;
- capture ignored files, config, hooks, or reflogs;
- provide compatibility with Jujutsu or another operation-log format;
- promise cross-platform path equivalence.

Anyone who can modify `.git/git2/` can delete history or replace HEAD with another valid operation. SHA-256 detects accidental corruption and name/content mismatch; it is not an authorization system.

## Identifiers and fanout

Content, views, and operations use SHA-256 over the exact stored bytes. IDs are displayed as 64 lowercase hexadecimal characters.

For an ID:

```text
0123456789abcdef...64 characters total...
```

the corresponding path is:

```text
<namespace>/01/23456789abcdef...remaining 62 characters...
```

The first byte of hexadecimal output provides a 256-directory fanout. Git SHA-1 IDs and recovery SHA-256 IDs are separate types and must never be substituted for one another.

## Top-level files

### `format`

Exact bytes for the current format:

```text
1\n
```

An existing different value is refused as an unsupported repository. Creating the store writes this file atomically.

### `HEAD`

`HEAD` contains the full operation ID as lowercase hexadecimal followed by one newline:

```text
<64 hex>\n
```

It points to an entry in `operations/`, not directly to a view. The selected operation’s `after` field identifies the current recovery view.

`set_head` refuses to select an operation whose immutable record does not exist.

### `lock`

`lock` is opened as a regular file and protected with a non-blocking exclusive advisory file lock. The file may remain after a process exits; lock ownership is associated with the open file descriptor and is released by the operating system.

The lock coordinates git2 writers only. Ordinary Git does not acquire it.

## Primitive encoding

Unless stated otherwise:

- integers are unsigned 64-bit little-endian values;
- signed timestamps are signed 64-bit little-endian values;
- IDs inside binary records are raw 32-byte SHA-256 digests;
- byte strings are `u64 length` followed by exactly that many bytes;
- counts are `u64` values followed by that many records;
- no alignment or padding bytes are inserted.

Decoder limits:

- maximum byte-string field length: 16 MiB;
- maximum record count in one counted collection: 10,000,000;
- counts and lengths must fit the current platform’s `usize`;
- trailing bytes after the declared record are rejected.

The parser applies these limits before allocating decoded field vectors, but the current store reads an entire view/operation file into memory before parsing. There is no total encoded-record file-size bound yet. Encoders also do not currently preflight their values against these decoder limits. Both gaps are release blockers: a sufficiently large ref value could be encoded into a view that the current decoder then refuses.

## CAS records

`cas/` stores raw payload bytes with no header or compression. The filename is the SHA-256 digest of those exact bytes.

Current CAS consumers:

- raw Git index bytes;
- regular file content;
- executable file content;
- symlink target bytes.

Reading recomputes SHA-256 and rejects a mismatch. Writing an ID that already exists verifies that the existing bytes are identical; different existing content is treated as corruption.

## View records

### Purpose

A view describes one complete supported local state. It references large bytes through CAS IDs.

### Magic and version

Every view starts with these 11 bytes:

```text
67 69 74 32 2d 76 69 65 77 00 01
 g  i  t  2  -  v  i  e  w NUL v1
```

In source notation the magic is `b"git2-view\0\x01"`.

### Layout

```text
magic:                  11 bytes
head:                   bytes
ref_count:              u64
refs[ref_count]:
  name:                 bytes
  value:                bytes
index_marker:           u8
index_id:               32 bytes, only when marker = 1
file_count:             u64
files[file_count]:
  path:                 bytes
  kind:                 u8
  content_id:           32 bytes
end of record
```

The only valid index markers are:

| Value | Meaning |
| --- | --- |
| `0` | No index existed |
| `1` | A raw index exists at `index_id` in CAS |

File kind codes:

| Value | Meaning | CAS payload |
| --- | --- | --- |
| `1` | Regular file | Exact file bytes |
| `2` | Executable file | Exact file bytes |
| `3` | Symbolic link | Link-target path bytes |

Ref records contain paths relative to the Git directory, such as `refs/heads/main`, plus exact file bytes. If `.git/packed-refs` exists, it is represented by the name `packed-refs` and its exact bytes. Ref records are sorted by byte name during capture.

File records are generated from a path-sorted map. Paths use `/` separators in Git/index form and are interpreted as Unix bytes by the current repository implementation.

### Captured absence

A missing tracked worktree path is omitted from `files`; the captured raw index still describes its staged/tracked state. A future restore must compare the target view with the current eligible namespace and remove current paths absent from the target. That restore logic is not implemented yet.

## Operation records

### Purpose

An operation is an immutable edge between views and forms a singly linked history through parent operation IDs.

### Magic and version

Every operation starts with these 16 bytes:

```text
67 69 74 32 2d 6f 70 65 72 61 74 69 6f 6e 00 01
 g  i  t  2  -  o  p  e  r  a  t  i  o  n NUL v1
```

In source notation the magic is `b"git2-operation\0\x01"`.

### Layout

```text
magic:                  16 bytes
parent_marker:          u8
parent_operation_id:    32 bytes, only when marker = 1
before_view_id:         32 bytes
after_view_id:          32 bytes
timestamp_seconds:      i64
kind:                   u8
command:                bytes, only when kind = 3
end of record
```

Valid parent markers:

| Value | Meaning |
| --- | --- |
| `0` | Root operation; no parent ID follows |
| `1` | A 32-byte parent operation ID follows |

Operation kind codes:

| Value | Meaning | Extra data |
| --- | --- | --- |
| `1` | Adoption baseline | None |
| `2` | State imported from ordinary Git/external edits | None |
| `3` | git2 command | One length-prefixed command byte string |

Timestamps are whole seconds relative to the Unix epoch. They are descriptive, not part of ordering; parent links define order. Times before the epoch are represented as negative values.

An adoption operation has no parent and currently uses the same view ID for `before` and `after`. An external import points to the previous selected operation, uses its current view as `before`, and the newly captured view as `after`.

## Atomicity and durability

Immutable and pointer writes follow this pattern:

1. Create the parent directory.
2. Create a unique temporary file with `create_new`.
3. Write all bytes.
4. `fsync` the file.
5. Rename it to the destination.
6. Attempt to `fsync` the parent directory.

Temporary names contain the process ID and an in-process monotonic counter. If another writer wins an immutable-record race, the loser removes its temporary file and verifies the destination bytes.

On Unix, rename replaces an existing mutable pointer such as `HEAD` atomically. The advisory lock prevents cooperating git2 writers from racing that update.

Directory `fsync` errors are currently ignored after a successful file rename. Release hardening must decide and test the durability contract across supported filesystems.

## Reachability

The reachable history is:

```text
HEAD
  → operation
      → before view
      → after view
      → parent operation
          → ...
```

Views reference CAS content. The store may contain unreachable CAS, view, or operation records after an interrupted write or deduplication race. There is no garbage collector; unreachable immutable data is harmless except for disk use.

Future garbage collection must begin from HEAD, walk all parent operations, retain both before and after views, retain every CAS ID referenced by those views, report the deletion plan, and never prune silently.

## Pending transactions: required design

Before any user-facing mutation is enabled, `pending/` must gain a versioned durable record containing at least:

- parent operation ID;
- complete pre-operation view ID;
- command kind and arguments needed for diagnostics;
- start timestamp;
- transaction phase;
- optional expected ref/index identities for compare-and-swap checks.

The pending record must be written and synced before Git state changes. Startup recovery must be idempotent. It should either:

- prove no mutation occurred, remove the pending record, and retain the original HEAD; or
- capture the resulting supported state, append a completed/interrupted command operation, move HEAD, and remove the pending record; or
- refuse with precise manual recovery instructions if a complete capture is impossible.

The exact pending encoding is intentionally unspecified until failure-injection tests drive it.

## Restore requirements

The format contains enough modeled data for a supported restore, but restore is not implemented or proven. A correct implementation must:

- verify every operation, view, and CAS digest before mutation;
- validate every path component;
- avoid following worktree symlinks;
- snapshot the current state first;
- write refs and index transactionally;
- restore file/symlink type and executable semantics;
- remove eligible current paths absent from the target;
- preserve ignored files;
- append a new restore/undo operation;
- remain recoverable after interruption at each boundary.

## Format-change checklist

Any recovery-format change must include:

1. Updated constants and encode/decode tests.
2. Golden binary fixtures for the old and new versions.
3. This document updated byte for byte.
4. A compatibility decision: read old, migrate explicitly, or refuse.
5. Corruption, truncation, oversized-length, and trailing-byte tests.
6. A changelog entry.
7. Review of reachability and garbage-collection implications.
