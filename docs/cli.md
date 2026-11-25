# CLI reference

The current binary exposes only implemented commands. Planned commands are documented in the roadmap, not accepted as no-op placeholders.

## Global behavior

```text
git2 <COMMAND>
git2 --help
git2 --version
```

Errors are written to standard error and return a non-zero exit status. Successful data/output is written to standard output. Repository discovery starts at the current directory and walks upward.

The supported repository commands are available on Unix builds. Linux is the tested target.

## `git2 adopt`

```text
git2 adopt [--max-file-mib <MiB>]
```

Creates the first private recovery baseline in `.git/git2/`.

Captured state:

- raw HEAD;
- loose refs and packed refs;
- raw index, if present;
- present tracked files;
- present non-ignored untracked files.

Options:

- `--max-file-mib <MiB>`: per-regular-file capture limit, default `100`, maximum `1024`.

The command refuses an already adopted repository. It also refuses unsupported repository states and eligible files over the limit. On success it prints the captured file count, eligible worktree-content bytes before deduplication, and full recovery view ID. The byte count does not include the raw index or view/operation metadata.

The selected size limit is stored in `.git/git2/policy`. Later automatic imports performed by `status` or `oplog` use that same limit.

Format 1 sidecars are refused, not upgraded or removed. To start a new format 2 store, first preserve the old directory separately; see the [format notes](recovery-format.md#status-and-compatibility).

The command does not make restore available; adoption is currently useful for format and capture testing only.

## `git2 status`

```text
git2 status
git2 status -z
```

Shows a path-sorted two-column status derived from HEAD → index and index → worktree comparisons.

The internal status model uses familiar porcelain codes:

| Code | Meaning |
| --- | --- |
| `A` | Added |
| `M` | Modified |
| `D` | Deleted |
| `T` | Type changed |
| `R` | Renamed |
| `C` | Copied |
| `U` | Unmerged |
| `??` | Untracked |

The current command does not enable rename/copy detection, so those changes normally appear as deletion/addition pairs. Repository validation also refuses an unresolved multi-stage index before status, so `U` is modeled but not currently part of the supported CLI path.

The first column is the index relative to HEAD; the second is the worktree relative to the index.

Options:

- `-z`, `--nul`: terminate each record with NUL. Path bytes are not quoted or transcoded and this is the required mode for robust scripting.

On an adopted repository, `status` first imports a changed current state as one `external-git` recovery operation. On an unadopted supported repository, it only calculates status.

The newline-delimited form also emits raw path bytes. A path containing a newline therefore makes that form ambiguous; use `-z`.

## `git2 oplog`

```text
git2 oplog
```

Requires an adopted repository. It imports external changes if needed, then prints immutable operations newest first.

Current output fields:

```text
<12-char-operation-id> <unix-seconds> <kind> <12-char-before-view> -> <12-char-after-view>
```

Kinds currently include:

- `adopt`;
- `external-git`;
- `git2:<command>` in the format, reserved for future mutating commands.

Output is intentionally simple and not yet declared a stable porcelain format.

## `git2 debug hash-object`

```text
git2 debug hash-object <path>
git2 debug hash-object --stdin
git2 debug hash-object --write <path>
git2 debug hash-object -w --stdin
```

Hashes bytes as a Git blob and prints the full SHA-1 ID. Without `--write`, no repository is required. With `-w`/`--write`, the command discovers a repository and atomically writes a loose blob object.

`--stdin` conflicts with a path. Only blob hashing is exposed.

This command writes directly to Git’s object database and does not create a recovery operation. Creating an unreachable blob is normally harmless, but this is a development/debug interface.

## `git2 debug cat-object`

```text
git2 debug cat-object <full-sha1>
git2 debug cat-object --type-only <full-sha1>
```

Reads a loose or packed object from the discovered repository. By default it writes raw object payload bytes to standard output.

Options:

- `-t`, `--type-only`: print `blob`, `tree`, `commit`, or `tag` instead of payload bytes.

Only full 40-character object IDs are accepted.

## Planned commands

The following names are roadmap commitments, not current CLI features:

```text
git2 add
git2 commit
git2 log
git2 reset
git2 restore
git2 undo
```

They will appear only after their transaction, interoperability, and failure-injection exit criteria pass.
