# Compatibility and limitations

This document distinguishes what the code accepts today from what the project may support later. “Representable” means a low-level codec understands the data; it does not mean repository-level recovery supports it.

## Platform matrix

| Platform | Object debug commands | Repository/recovery commands | Test status |
| --- | --- | --- | --- |
| Linux | Available | Available | Active target |
| macOS | Expected to compile as Unix | Experimental and unverified | Native CI required after v0.1 |
| Windows | Object layer only | Compiled out | Planned after v0.1 |

The repository and status modules intentionally use Unix byte-path and permission APIs today.

## Repository matrix

| Repository feature | Current behavior | Rationale / future gate |
| --- | --- | --- |
| SHA-1 objects | Supported | Current Git object implementation |
| SHA-256 objects | Refused | Object ID and tree encoding work required |
| Non-bare main worktree | Supported | v0.1 target |
| Bare repository | Refused | No worktree recovery semantics |
| Single worktree | Supported | v0.1 target |
| Linked worktrees | Refused | Shared refs/common-dir transaction design required |
| Separate Git directory or metadata outside `.git` | Refused | Git metadata must live in the worktree's own real `.git` directory |
| Alternate object databases | Refused | Borrowed objects cannot be vouched for locally |
| Shallow clones | Refused | `shallow` file detected; borrowed history breaks complete capture |
| Reftable reference storage | Refused | Only file-backed loose and packed refs are captured |
| Split index | Refused at discovery and again at capture | The raw top-level index is not self-contained; the `link` extension is refused |
| Sparse index/checkout | Refused at discovery and again at capture | Sparse markers, `SKIP_WORKTREE` flags, or `core.sparseCheckout` config detected |
| Partial/promisor clone | Refused when promisor packs, `remote.*.promisor`, or `extensions.partialClone` config are detected | Missing objects violate complete local capture assumptions |
| Submodules/gitlinks | Refused at repository level | Nested repository state not captured |
| Gitlinks in historical trees reached by retention | Refused at capture | Reachability traversal refuses submodule-linked trees |
| Index with resolve-undo (REUC) records | Refused at capture | Retention does not model resolve-undo entries |
| Nested repository under the worktree | Refused at capture | Not a capturable file tree, including bare nested repositories |
| Unresolved index conflicts | Refused | Multi-stage index restoration not implemented |
| In-progress merge | Refused | `MERGE_HEAD` detected |
| In-progress rebase | Refused | `rebase-apply`/`rebase-merge` detected |
| Cherry-pick/revert/sequencer | Refused | Operation state files detected |
| Bisect state | Refused when `BISECT_LOG` exists | Not in v0.1 scope |

Detection is conservative but not yet exhaustive. Unknown extensions or concurrent changes may still surface as lower-level errors. Expanding support requires restore and failure tests, not merely removing a refusal.

## Object compatibility

| Object behavior | Status |
| --- | --- |
| Full 40-hex SHA-1 parsing | Implemented |
| Canonical blob hashing | Implemented and compared with Git |
| Loose object zlib read/write | Implemented with hash verification and atomic write |
| Packed object read | Implemented through `gix`, tested after `git gc` |
| Commit parsing/serialization | Implemented, including byte messages and multiline extra headers |
| Tree parsing/serialization | Implemented, including non-UTF-8 names and gitlink modes |
| Annotated tag-specific model | Not implemented; raw tag objects are readable |
| Abbreviated object IDs | Not implemented in CLI |
| Object writes | `git2 add` writes staged loose blobs via `.git/index.lock` transactions; `git2 restore`/`recover` rehydrate retained objects; `debug hash-object` stays read-only |
| Retained object closure | Implemented for refs/HEAD/index roots with tag/commit/tree traversal; writes no public refs or objects |

## Captured state

| State | Captured | Restorable today |
| --- | --- | --- |
| Raw HEAD | Yes | Yes (`restore --view`) |
| Loose refs under `refs/` | Yes | Yes (`restore --view`, including D/F swaps) |
| Raw `packed-refs` | Yes | Yes (`restore --view`) |
| Raw index | Yes, checksum-verified with a size-bounded decode and semantic validation at capture too | Yes (`restore --view`) |
| Present tracked regular files | Yes, with the on-disk mode | Yes (`restore --view`) |
| Present tracked executable files | Yes, with the on-disk mode even when the index disagrees | Yes (`restore --view`) |
| Present tracked symlinks | Yes, as the raw target without dereference | Yes (`restore --view`) |
| Non-ignored untracked files | Yes, through a checked walk that never applies filters | Yes (`restore --view`) |
| Git object closure reachable from saved refs/HEAD/index | Yes, on adoption, automatic imports, and explicit checkpoints, in the private CAS | Yes (verified and rehydrated before publishing refs/index) |
| Ignored files | No | No (preserved untouched; obstructing ignored children refuse restore) |
| Deleted tracked path absence | Represented by omission plus captured index | Yes (extra eligible files removed by `restore --view`) |
| Empty directories | No | No (pruned on file removal unless holding kept ignored entries) |
| Git config/hooks/reflogs | No | No |
| File ownership/ACL/xattrs/timestamps | No | No |

Captured file kinds follow the worktree, never the index: a staged executable whose
execute bit is cleared on disk is captured as a regular file. Reads are descriptor-relative
from one open worktree root: on Linux, parent directories are opened `O_PATH` for
traversal only, so searchable-but-unreadable directories remain usable; parent components
are opened without following symlinks (a symlinked parent is refused, and a regular
ancestor is deliberately kept an access error rather than being recast as an absent
path), symlink leaves are read with `readlinkat` and never opened, and only regular files
are ever opened, non-blocking, with reads bounded by the configured limit at read time and
re-checked for size drift so a file that changes size mid-read fails instead of being
persisted as a truncated prefix. Refusals happen before the recovery sidecar is created,
and the index semantics validated at discovery are validated again on the bytes captured.

## Size and storage limits

- Default maximum eligible regular-file size: 100 MiB.
- `git2 adopt --max-file-mib N` sets a persisted limit of up to 1024 MiB; automatic imports use the same limit.
- A baseline containing at least 5 GiB produces a warning.
- Index capture verifies nonzero raw checksums (Git can explicitly omit the checksum) and bounds decode allocations by four times the index's own size.
- Recovery fields are bounded to 16 MiB when decoding.
- Record counts are bounded to 10,000,000 when decoding.
- Whole recovery views are bounded to 64 MiB, operations to 1 MiB, and CAS payloads to 1 GiB.
- Each retained Git object payload is bounded to 128 MiB; packed delta chains are limited to depth 64 with a 128 MiB aggregate inflated-instruction budget. Decoding can also use two object-sized working buffers.
- Retention keeps at most 1,000,000 distinct Git objects, further bounded by the 64 MiB whole-view limit.
- New format 3 sidecars are owner-private and store CAS payloads zlib-compressed; format 1 and format 2 sidecars are refused without migration.
- There is no automatic garbage collection or pruning.
- Capture currently reads each eligible file into memory before persisting it.

The configurable file limit is a safety valve, not a claim of efficient large-file support.

## Path and content behavior

Unix paths and Git path data are treated as bytes. `git2 status -z` is the safe interface for names containing newlines or invalid UTF-8. The newline-delimited form writes raw path bytes and is intended for human inspection only.

Captured filesystem entries may be regular, executable, or symlink. Directories and special files such as devices, sockets, and FIFOs are rejected when encountered as eligible entries, and a symlinked parent directory is rejected rather than followed.

## Filters, attributes, and ignores

Eligible-untracked enumeration uses a checked `gix` directory walk that applies repository
ignore rules and classifies nested repositories and special entries, but never runs
tracked-file content conversion or configured executable filters. Ignored files are
deliberately excluded from the v0.1 guarantee; ignored nested repositories and special
entries stay outside it too.

`git2 add` applies built-in `.gitattributes`, `core.autocrlf`, `core.eol`, and ignore/literal path rules via `gix` and refuses active external `filter=` clean/process drivers and non-UTF-8 `working-tree-encoding`. Similarly, `git2 reset --hard` applies built-in checkout EOL conversion via `gix` and refuses active external smudge/process filters.

## Concurrency limitations

An advisory lock excludes a second cooperating git2 writer. Ordinary Git does not use this lock. There is no background monitor, and concurrent Git/worktree changes during capture are not yet guarded by a full consistency/retry protocol.

## Known alpha risks

- Native `commit` and `log` are not implemented yet.
- Captures and bounded recovery reads still use memory. This is not a streaming or large-repository backup.
- Retention saves only the Git object closure available at capture time, and ordinary Git can still prune objects that were never captured.
- git2 does not monitor ordinary Git in the background. Outside changes are imported only when `status`, `oplog`, `checkpoint`, `restore`, `reset`, `undo`, or `add` runs, and a checkpoint is a capture rather than a backup.
- Ordinary Git can race capture despite the git2 advisory lock.
- Recovery-store sync errors are reported and process crash failpoints are tested across every transaction phase, though hardware power-loss testing is not yet implemented.
- Object-not-found classification in packed fallback relies partly on dependency error text and needs a typed error path.
- Human newline status output does not implement Git’s quoting rules; `-z` is the only robust byte-path interface.

These risks are reasons for the repository-wide experimental warning, not merely documentation caveats.

## Networking

Clone, fetch, pull, and push are absent. The planned first networking implementation delegates to the installed Git executable and verifies that `.git/git2/` remains private. Native protocol work is not on the v0.1 critical path.
