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
- present non-ignored untracked files;
- the Git object closure reachable from those saved refs, HEAD, and index (see below).

Options:

- `--max-file-mib <MiB>`: per-regular-file capture limit, default `100`, maximum `1024`.

The command refuses an already adopted repository. It also refuses unsupported repository states and eligible files over the limit. On success it prints the captured file count, eligible worktree-content bytes before deduplication, and full recovery view ID. The byte count does not include the raw index, retained Git objects, or view/operation metadata.

The selected size limit is stored in `.git/git2/policy`. Later checkpoints and automatic imports performed by `status` or `oplog` use that same limit.

Format 1 sidecars are refused, not upgraded or removed. To start a new format 2 store, first preserve the old directory separately; see the [format notes](recovery-format.md#status-and-compatibility).

The command does not make restore available; adoption is currently useful for format and capture testing only.

## `git2 checkpoint`

```text
git2 checkpoint
```

Requires an adopted repository. Explicitly captures the current supported state and prints the full 64-character lowercase-hex recovery view ID:

```text
checkpointed <file-count> files (<bytes> bytes) as view <64-hex-view-id>
```

Behavior:

- If the supported state changed since the last capture, it writes a new immutable view and appends one `git2:checkpoint` operation whose `before` is the previously selected view.
- If nothing changed, it returns the same view ID as the current view and appends no operation.
- It requires an adopted repository and refuses before creating a sidecar in an unadopted one.
- git2 does not monitor ordinary Git in the background. A checkpoint is an explicit snapshot taken when the command runs; outside edits are otherwise imported by `status` or `oplog` as `external-git` operations.

Like adoption and automatic imports, a checkpoint also retains the current Git object closure in the private sidecar: OIDs from the physical loose and packed refs (including values shadowed by a newer loose ref and peeled OIDs in `packed-refs`), the object named by HEAD (or an unborn branch with no object), concrete stage-zero index blobs, and valid cache-tree entries. It follows tag, commit, and tree edges to retain their referenced objects. Retention only reads Git objects and writes canonical bytes into the private CAS; it does not create or change public refs or Git objects.

Scope and limits: checkpoint refuses indexes with resolve-undo (REUC) records and historical trees containing gitlink entries. Each retained object payload is bounded to 128 MiB, the map to 1,000,000 distinct Git objects, packed delta chains to depth 64 with a 128 MiB aggregate instruction budget, and the whole view to 64 MiB. A required object that is missing or over a limit refuses the checkpoint before a new view is exposed.

Earlier format 2 views written before retention shipped have an empty retained-object map. They remain readable but are not retroactively completed; a new checkpoint can only retain objects that are still available in the repository at the time it runs. The on-disk format is unchanged. Checkpoint is a capture, not a backup: there is no restore command or pending-transaction journal yet.

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

On an adopted repository, `status` first imports a changed current state as one `external-git` recovery operation, retaining the current Git object closure in the sidecar. On an unadopted supported repository, it only calculates status.

The newline-delimited form also emits raw path bytes. A path containing a newline therefore makes that form ambiguous; use `-z`.

## `git2 oplog`

```text
git2 oplog
```

Requires an adopted repository. It imports external changes if needed (retaining the current Git object closure, like `status` and `checkpoint`), then prints immutable operations newest first.

Current output fields:

```text
<12-char-operation-id> <unix-seconds> <kind> <12-char-before-view> -> <12-char-after-view>
```

Kinds currently include:

- `adopt`;
- `external-git`;
- `git2:checkpoint`, emitted by `git2 checkpoint`;
- other `git2:<command>` values in the format, reserved for future mutating commands.

An unchanged `checkpoint` does not append a new operation, so repeated identical saves leave `oplog` unchanged.

Output is intentionally simple and not yet declared a stable porcelain format.

## `git2 debug hash-object`

```text
git2 debug hash-object <path>
git2 debug hash-object --stdin
```

Hashes bytes as a Git blob and prints the full 40-character SHA-1 ID. This command is read-only: no repository is required and no object is written. `--stdin` conflicts with a path. Only blob hashing is exposed.

## `git2 debug cat-object`

```text
git2 debug cat-object <full-sha1>
git2 debug cat-object --type-only <full-sha1>
```

Reads a loose or packed object from the discovered repository. This command is read-only; by default it prints raw object payload bytes to standard output.

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
