# Recovery format

## Status and compatibility

The recovery sidecar format is currently version 3 and is experimental. Format 1 and format 2 stores are refused in place; git2 does not migrate, delete, or overwrite them. If you want to adopt again, first archive the entire existing `.git/git2/` directory separately while nothing is using it. All v0.1 recovery, local mutation, and delegated remote commands (`adopt`, `checkpoint`, `status`, `oplog`, `restore --view <id> [--dry-run]`, `recover [--status | --rollback]`, `reset`, `undo`, `add`, `commit`, `log`, `remote`, `clone`, `fetch`, `pull`, `push`) operate over format 3 stores.

The reader also refuses unknown top-level versions. Any incompatible change must increment that version and update this document and fixtures. Format 3 keeps the format 2 binary view, operation, and policy record layouts intact and adds RFC 1950 zlib compression to on-disk `cas/` entries. git2 does not monitor ordinary Git in the background.

If initialization or adoption is interrupted and the store has no format, policy, or HEAD, git2 reports the missing entry. Preserve the incomplete directory separately before trying adoption again; git2 does not automatically remove it.

The format is private to one clone. It is stored below the Git directory and never creates refs:

```text
.git/git2/
  format
  policy
  HEAD
  lock
  cas/<2 hex>/<62 hex>
  views/<2 hex>/<62 hex>
  operations/<2 hex>/<62 hex>
  pending/
    active
    plan
    delegated
    stage_<16hex>_<index>
```

`pending/` holds either a native 4-phase transaction (`active`, `plan`, and transient same-filesystem lock-staging files `stage_<16hex>_<index>`) or a delegated Git-subprocess transaction (`delegated`). While no transaction is running or awaiting reconciliation, `pending/` is empty.

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

Version 3 does not:

- encrypt recovery content;
- authenticate content against a secret key;
- deltify CAS payloads across records;
- garbage-collect unreachable records;
- preserve ACLs, ownership, xattrs, or timestamps;
- capture ignored files, config, hooks, or reflogs;
- monitor ordinary Git in the background — checkpoints are explicit captures;
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
3\n
```

An existing different value is refused as an unsupported repository. Creating the store writes this file atomically. The marker is the only file read before checking for owner-private permissions so that older stores (`1\n` or `2\n`) can be identified without modifying them.

### `policy`

Exact layout: the 13-byte magic `b"git2-policy\0\x01"` followed by the eligible regular-file limit as an unsigned 64-bit little-endian integer. The default is 104,857,600 bytes (100 MiB); the maximum is 1,073,741,824 bytes (1 GiB). Zero is allowed. The policy is immutable and applies to later automatic imports as well as adoption.

### `HEAD`

`HEAD` contains the full operation ID as lowercase hexadecimal followed by one newline:

```text
<64 hex>\n
```

It points to an entry in `operations/`, not directly to a view. The selected operation’s `after` field identifies the current recovery view.

`set_head` verifies the selected operation, its immediate parent when present, and its before/after views before publishing the pointer. It does not walk the entire history or verify every referenced CAS payload.

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

Before allocation, decoders also verify that each record count is plausible for bytes remaining. Whole views are limited to 64 MiB and operations to 1 MiB; short control files have tighter limits. CAS payloads are capped at 1 GiB independent of the configured per-worktree-file limit. Readers check size before and during I/O and check digests after reading; encoders preflight field/count and total-size limits before building output. Capture still materializes file bytes in memory, and these checks do not make it a streaming backup tool.

## CAS records

`cas/` stores RFC 1950 zlib-compressed payload bytes on disk. The filename is the SHA-256 digest of the **uncompressed** payload bytes, not of the compressed stream.

Current CAS consumers:

- raw Git index bytes;
- regular file content;
- executable file content;
- symlink target bytes;
- canonical bytes of retained Git objects (blob, tree, commit, and tag payloads with their framing).

Reads bound the compressed file size (`limit + 1 MiB`), inflate incrementally with a strict `limit` cap on uncompressed bytes, reject truncated streams (including truncated Adler-32 trailers) and any trailing bytes after the zlib stream end, and recompute SHA-256 over the uncompressed bytes to reject mismatches. Writing an ID whose `cas/` entry already exists reads and decompresses the existing record and verifies that its uncompressed bytes match byte for byte; different existing content is treated as corruption.

## View records

### Purpose

A view describes one complete supported local state. It references large bytes through CAS IDs.

### Magic and version

Every view starts with these 11 bytes:

```text
67 69 74 32 2d 76 69 65 77 00 02
 g  i  t  2  -  v  i  e  w NUL v2
```

In source notation the magic is `b"git2-view\0\x02"`.

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
retained_object_count:   u64
retained_objects[retained_object_count]:
  oid:                   20 bytes (raw Git SHA-1)
  content_id:            32 bytes
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

The retained-object map is ordered by strictly increasing Git object ID, so duplicates are invalid. It is populated by adoption, automatic imports, and explicit checkpoints with the Git object closure described below, and verified before `restore` mutates repository state.

### Retained Git object map

Roots are derived from the *saved* metadata only, not from live refs:

- every physical loose ref and every value in the saved `packed-refs`, including values shadowed by a newer loose ref and peeled OIDs recorded in `packed-refs`;
- the object named by HEAD when it resolves directly, while a valid unborn `refs/heads/` HEAD contributes no object;
- concrete stage-zero index entries (regular, executable, and symlink modes), while intent-to-add placeholder IDs contribute no object;
- valid cache-tree entries (`TREE` extensions that carry an entry count).

Traversal follows tag → target, commit → tree and parents, and tree → subtrees and blobs. The view maps each Git SHA-1 ID to the SHA-256 CAS ID of its canonical bytes. A missing required object, a kind mismatch, or an over-limit object refuses the capture before the new view is published. Indexes with resolve-undo (`REUC`) records are refused, as are historical trees containing gitlink (submodule) entries. Each retained object payload is bounded to 128 MiB, the map to 1,000,000 distinct Git objects, packed delta chains to depth 64 with a 128 MiB aggregate inflated-instruction budget, and the encoded whole view to 64 MiB. Retention reads only local loose and indexed pack files without applying replacement refs. It does not write public refs or objects, and it does not touch Git's index or worktree. A failed capture may leave unreferenced private CAS data (or an incomplete store on first adoption), but it does not select a partial view.

### Captured absence

A missing tracked worktree path is omitted from `files`; the captured raw index still describes its staged/tracked state. `git2 restore` compares the target view with the current eligible namespace and removes current eligible paths absent from the target while preserving ignored files.

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
| `3` | `git2` command (`checkpoint`, `restore`, `reset`, `add`, `commit`, `undo`, `fetch`, `pull`, `push`, `remote`; rendered by `oplog` as `git2:<command>`) | One length-prefixed command byte string |

Timestamps are whole seconds relative to the Unix epoch. They are descriptive, not part of ordering; parent links define order. Times before the epoch are represented as negative values.

An adoption operation has no parent and currently uses the same view ID for `before` and `after`. An external import points to the previous selected operation, uses its current view as `before`, and the newly captured view as `after`. An explicit checkpoint uses kind 3 with the command byte string `checkpoint`; when the state changed it points to the previous selected operation with the current view as `before` and the new view as `after`, while an unchanged checkpoint appends no operation and reuses the current operation and view. Embedded remote URL credentials (`scheme://user:secret@host`) are redacted (`https://***@host`) before persisting delegated command strings, and `git2 undo` explicitly refuses operations whose command is `push` or begins with `push `.

## Atomicity and durability

Sidecar directories are created with mode `0700` and files with `0600` (a stricter process umask can remove permissions). Git2 checks ownership against the effective user and rejects group/other access to v3 store entries. It checks file types and uses descriptor-relative opens that do not follow symlinks inside the store.

Immutable records and mutable pointers follow this pattern:

1. Create the parent directory.
2. Create a unique temporary file with `create_new`.
3. Write all bytes.
4. `fsync` the file.
5. Publish an immutable record without replacing an existing entry, or rename a mutable pointer into place.
6. `fsync` the parent directory. Sync parents of newly created directories as well.

Temporary names contain the process ID and an in-process monotonic counter. If another writer wins an immutable-record race, the loser removes its temporary file and verifies the destination bytes.

On Unix, rename replaces an existing mutable pointer such as `HEAD` atomically. The advisory lock prevents cooperating git2 writers from racing that update.

File and directory sync failures are reported as errors. A failure after publication can leave the new entry visible without a durability guarantee. This is not a mutation journal or proof of recovery after power loss.

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

## Pending transactions

The transaction layer (`crates/git2-core/src/transaction.rs`) drives two pending-state formats under `.git/git2/pending/`:

- a constrained 4-phase state machine (`Prepared -> Applying -> Finalizing -> Committed`) over 11 fixed primitive checked actions (`pending/active` + `pending/plan`) for local mutations;
- a delegated subprocess journal (`PendingPhase::Delegated = 5`, `pending/delegated`) for `git`-delegated network and remote-tracking mutations (`fetch`, `pull`, `push`, `remote remove`, `remote rename`).

### `pending/active` (`b"git2-pending\0\x01"`)

Whole record limit: 1 MiB (`MAX_OPERATION_BYTES`), atomically replaced via staged temp write + `fsync` + `renameat` + directory `fsync`.

```text
magic:              14 bytes ("git2-pending\0\x01")
tx_id:              32 bytes (SHA-256 of parent, before_view, target_view, plan_id, timestamp, command)
parent_operation:   32 bytes
before_view:        32 bytes
target_view:        32 bytes
plan_id:            32 bytes (SHA-256 of pending/plan)
timestamp_seconds:  i64
command:            bytes
phase:              u8 (1=Prepared, 2=Applying, 3=Finalizing, 4=Committed)
next_action:        u64
lock_owner_marker:  u8 (0=None, 1=Present)
  when marker = 1:
    action_index:   u64
    dev:            u64 (st_dev of staged lock inode)
    ino:            u64 (st_ino of staged lock inode)
    lock_rel_path:  bytes (relative to .git/, e.g. "index.lock")
    stage_name:     bytes (filename inside pending/, e.g. "stage_<16hex>_<index>")
end of record
```

### `pending/plan` (`b"git2-plan\0\x01"`)

Whole record limit: 64 MiB (`MAX_VIEW_BYTES`). Written and synced before `pending/active` is published.

```text
magic:              11 bytes ("git2-plan\0\x01")
action_count:       u64
actions[action_count]:
  tag:              u8
  payload:          tag-specific fields
end of record
```

Action tags:

| Tag | Variant | Payload |
| --- | --- | --- |
| `1` | `RehydrateObject` | `oid: 20 bytes`, `content: 32 bytes` |
| `2` | `WriteWorktreeEntry` | `path: bytes`, `expected_before: u8 marker + (kind: u8, content: 32B)`, `target: (kind: u8, content: 32B)` |
| `3` | `RemoveWorktreeEntry` | `path: bytes`, `expected_before: (kind: u8, content: 32B)` |
| `4` | `EnsureDir` | `path: bytes` |
| `5` | `RemoveEmptyDir` | `path: bytes` |
| `6` | `WriteIndex` | `expected_before: u8 marker + optional 32B`, `target: 32 bytes` |
| `7` | `RemoveIndex` | `expected_before: 32 bytes` |
| `8` | `WriteRef` | `name: bytes`, `expected_before: u8 marker + optional bytes`, `target: bytes` |
| `9` | `RemoveRef` | `name: bytes`, `expected_before: bytes` |
| `10` | `WritePackedRefs` | `expected_before: u8 marker + optional bytes`, `target: u8 marker + optional bytes` |
| `11` | `WriteHead` | `expected_before: bytes`, `target: bytes` |

### `pending/delegated` (`b"git2-delegated\0\x01"`)

Whole record limit: 1 MiB (`MAX_OPERATION_BYTES`). Written and synced after capturing `before_view` and before spawning installed `git` for remote/network mutations. Note that in-memory `PendingPhase::Delegated = 5` is reported by `git2 recover --status` whenever `pending/delegated` is present (there is no `pending/active` or `pending/plan` file for delegated transactions).

```text
magic:                    16 bytes ("git2-delegated\0\x01")
tx_id:                    32 bytes (SHA-256 of parent, before_view, timestamp, command, allow_conflict_rollback)
parent_operation:         32 bytes
before_view:              32 bytes
timestamp_seconds:        i64
command:                  bytes (embedded URL userinfo redacted as "***")
allow_conflict_rollback:  u8 (0=False, 1=True)
end of record
```

Delegated recovery and rollback behavior:

- **Conflict detection:** `has_git_conflict_state` inspects `.git/` for `MERGE_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `rebase-merge`, `rebase-apply`, `sequencer`, or unmerged stage `!= 0` (or unparseable) index entries. If conflict state is present, plain `git2 recover` refuses and instructs the user either to resolve conflicts in Git and re-run `git2 recover` or to run `git2 recover --rollback`.
- **Clean/resolved `git2 recover`:** strips any optional `REUC` (resolve-undo) extension left in `.git/index` after manual Git conflict resolution (while refusing a foreign `index.lock`), captures `after_view`, returns `Aborted` if `after_view == before_view`, or writes and selects the `Operation` (`Git2(command)`) and removes `pending/delegated` idempotently.
- **Conflict-safe `git2 recover --rollback`:** refuses any foreign Git lockfiles, deletes transient Git state files (`MERGE_HEAD`, `MERGE_MSG`, `MERGE_MODE`, `AUTO_MERGE`, `ORIG_HEAD`, `FETCH_HEAD`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `BISECT_LOG`) and directories (`rebase-merge`, `rebase-apply`, `sequencer`), restores any invalid index back to `before_view.index`, applies the verified restore plan back to `before_view`, verifies `actual_before == before_view`, and removes `pending/delegated`.

### Lock ownership, temp debris, and recovery procedure

1. **Pre-write durability:** The runner acquires `.git/git2/lock`, checks for any active pending record (`pending/active` or `pending/delegated`), verifies `expected_head`, captures live state (publishing `external-git` if live state matches `before_view` and differs from `current_view`), writes `pending/plan`, and writes `pending/active` (`Prepared`) before touching any Git or worktree entry.
2. **Staged-inode Git lock attribution:** Lock-based Git metadata actions (`WriteIndex`, `RemoveIndex`, `WriteRef`, `RemoveRef`, `WritePackedRefs`, `WriteHead`) first create `pending/stage_<16hex>_<index>` (`O_CREAT | O_EXCL`), record its `(st_dev, st_ino)` in `pending/active` (`Applying`), and then hard-link (`linkat`, never replacing) to `<target>.lock`. On collision (`EEXIST`) or during recovery/rollback, git2 verifies `(st_dev, st_ino)` matches `pending/active`; any unknown lock is refused byte-for-byte untouched.
3. **Worktree temp debris:** Checked atomic worktree entry writes create `.git2-tmp-<16hex>-<index>` in the target's validated parent directory (`O_NOFOLLOW`, `fsync` + `renameat` + parent `fsync`). Recovery and rollback unlink any leftover `.git2-tmp-*` sibling before proceeding.
4. **Idempotent reconciliation (`recover`) and rollback (`rollback`):**
   - Neither `pending/active` nor `pending/delegated`: clean up any orphan `pending/plan` or `stage_*` debris and return `Clean`.
   - `pending/delegated` exists: reconcile or roll back using the delegated protocol above.
   - `HEAD` already equals deterministic `operation_id`: release any verified owned lock, clean temp/pending files, and return `Finalized` without appending duplicate operations.
   - `phase == Prepared`: recompute the current live view read-only without writes and verify it matches `before_view` (refusing unexpected divergence), clean pending files, and return `Aborted` (or `RolledBack` under `rollback`).
   - `phase == Applying | Finalizing | Committed` (with `HEAD == parent`): `recover` probes outcomes per action (resuming owned locks/unfinished actions, pruning empty ancestor directories under `refs/` on `RemoveRef`, and refusing foreign locks or divergence), verifies live state matches `target_view`, writes the deterministic `Operation`, publishes `HEAD`, cleans pending state, and returns `Finalized`. Alternatively, `rollback` walks reversible actions in reverse up to `next_action` (skipping append-only `RehydrateObject`), verifies `before_view`, cleans pending state, and returns `RolledBack`.

## Restore contract and recovery runbook

The `restore` planner and executor implement the modeled restoration checks:

- verify the target view, every target CAS payload digest, and the full retained Git object closure before touching `.git/` or the worktree;
- refuse pre-existing foreign Git locks on planned metadata targets before mutating worktree files;
- evaluate both current and target ignore profiles (`core.excludesFile` / XDG ignore, `.git/info/exclude`, and captured/live `.gitignore` rules) and refuse ignored-child directory obstructions or ambiguous ignored-to-untracked transitions without writes;
- rehydrate missing required Git objects before publishing refs or index, execute swaps without following symlinks or escaping the worktree, preserve ignored files, and append `git2:restore --view <id>`.

Operator recovery guidance:

- **Pending transaction (`pending/active` or `pending/delegated` exists):** run `git2 recover --status` (read-only) or `git2 oplog` (read-only warning mode), then run `git2 recover` to finish cleanly or `git2 recover --rollback` to return to `before_view` if recovery `HEAD` has not yet moved (and abort any conflicted delegated pull).
- **Unknown Git lock (`*.lock` mismatch):** inspect or stop any concurrent Git/editor process holding `.git/*.lock` before re-running `git2 recover`; git2 never deletes an unattributed lock.
- **Corrupted or legacy sidecar:** do not hand-edit `.git/git2/`; preserve `.git/git2/` externally for diagnosis (format 1/2 stores are never auto-migrated or removed).

## Format-change checklist

Any recovery-format change must include:

1. Updated constants and encode/decode tests.
2. Golden binary fixtures for the old and new versions.
3. This document updated byte for byte.
4. A compatibility decision: read old, migrate explicitly, or refuse.
5. Corruption, truncation, oversized-length, and trailing-byte tests.
6. A changelog entry.
7. Review of reachability and garbage-collection implications.
