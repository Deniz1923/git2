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
| Sparse index/checkout | Refused | Complete-view semantics not proven |
| Partial/promisor clone | Refused when promisor packs are detected | Missing objects violate complete local capture assumptions |
| Submodules/gitlinks | Refused at repository level | Nested repository state not captured |
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
| Object writes other than debug blobs | Not exposed yet |

## Captured state

| State | Captured | Restorable today |
| --- | --- | --- |
| Raw HEAD | Yes | No |
| Loose refs under `refs/` | Yes | No |
| Raw `packed-refs` | Yes | No |
| Raw index | Yes | No |
| Present tracked regular files | Yes | No |
| Present tracked executable files | Yes | No |
| Present tracked symlinks | Yes | No |
| Non-ignored untracked files | Yes | No |
| Ignored files | No | No |
| Deleted tracked path absence | Represented by omission plus captured index | No |
| Empty directories | No | No |
| Git config/hooks/reflogs | No | No |
| File ownership/ACL/xattrs/timestamps | No | No |

## Size and storage limits

- Default maximum eligible regular-file size: 100 MiB.
- `git2 adopt --max-file-mib N` changes that adoption limit.
- A baseline containing at least 5 GiB produces a warning.
- A non-default adoption limit is not persisted; later automatic imports use 100 MiB again.
- Recovery fields are bounded to 16 MiB when decoding.
- Record counts are bounded to 10,000,000 when decoding.
- There is no automatic garbage collection or pruning.
- Capture currently reads each eligible file into memory before persisting it.

The configurable file limit is a safety valve, not a claim of efficient large-file support.

## Path and content behavior

Unix paths and Git path data are treated as bytes. `git2 status -z` is the safe interface for names containing newlines or invalid UTF-8. The newline-delimited form writes raw path bytes and is intended for human inspection only.

Captured filesystem entries may be regular, executable, or symlink. Directories and special files such as devices, sockets, and FIFOs are rejected when encountered as eligible entries.

## Filters, attributes, and ignores

Current status and eligible-untracked enumeration use `gix` plumbing and repository ignore rules. Ignored files are deliberately excluded from the v0.1 guarantee.

Future staging must apply Git clean filters, attributes, line-ending rules, executable semantics, and ignore/pathspec behavior consistently with Git. Staging is not complete until differential fixtures cover these cases.

## Concurrency limitations

An advisory lock excludes a second cooperating git2 writer. Ordinary Git does not use this lock. There is no background monitor, and concurrent Git/worktree changes during capture are not yet guarded by a full consistency/retry protocol.

## Known alpha risks

- Restore has not been implemented, so captured views have not passed an end-to-end recovery proof.
- Recovery record files and CAS payloads are read into memory before parsing/hash verification. Field/count limits do not impose a total file-size bound.
- Encoders do not yet reject values over decoder field/count limits before writing. An unusually large ref value, such as a `packed-refs` file over 16 MiB, could produce a view that the current decoder refuses. This must be fixed before v0.1.
- The recovery directory uses normal process umask/parent permissions; git2 does not enforce or audit a private permission mode yet.
- Ordinary Git can race capture despite the git2 advisory lock.
- Directory-sync failures after rename are currently ignored.
- Object-not-found classification in packed fallback relies partly on dependency error text and needs a typed error path.
- Human newline status output does not implement Git’s quoting rules; `-z` is the only robust byte-path interface.

These risks are reasons for the repository-wide experimental warning, not merely documentation caveats.

## Networking

Clone, fetch, pull, and push are absent. The planned first networking implementation delegates to the installed Git executable and verifies that `.git/git2/` remains private. Native protocol work is not on the v0.1 critical path.
