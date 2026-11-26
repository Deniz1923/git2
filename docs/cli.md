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

Format 1 and format 2 sidecars are refused, not upgraded or removed. To start a new format 3 store, first preserve the old directory separately; see the [format notes](recovery-format.md#status-and-compatibility).

Captured baseline and checkpoint views can be previewed and applied with `git2 restore --view <id> [--dry-run]`.

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

Earlier format 1 and format 2 stores are refused untouched; format 3 keeps the version 2 view/operation layout and stores CAS entries zlib-compressed on disk. Saved views can be restored later with `git2 restore --view <id>`, which rehydrates missing objects from the private CAS and runs through the 4-phase pending-transaction journal.

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

## `git2 restore`

```text
git2 restore --view <64-hex-view-id> [--dry-run]
```

Requires an adopted repository and full 64-character lowercase-hex view ID from `adopt`, `checkpoint`, or `oplog`.

Options:

- `--view <64-hex-view-id>`: target recovery view to preview or apply;
- `--dry-run`: validate the target view, object closure, ignore rules, and obstructions without acquiring the recovery lock, importing external changes, or modifying Git/sidecar state. Emits one single-line byte-safe (`\"...\"`-escaped when needed) change line per affected object, worktree path, index, ref, `packed-refs`, or `HEAD`.

Behavior:

- refuses before writes if a pending transaction exists, any required target view/CAS payload/retained Git object is missing or corrupt, a pre-existing foreign Git `.lock` exists for a touched metadata path, an ignored child blocks directory replacement, or an ignore-rule change creates an ambiguous or unverified entry transition;
- allows non-destructive path reclassification (such as a captured entry becoming ignored in the target, or an on-disk ignored file already matching a target tracked file byte-for-byte and mode-for-mode) without unnecessary worktree writes, while refusing differing obstructing ignored files;
- on apply, imports outside changes if needed, runs through the 4-phase pending-transaction journal, rehydrates missing Git objects into `.git/objects/` before publishing refs or index, swaps files/directories and D/F loose refs safely, leaves ignored files untouched, and appends one `git2:restore --view <id>` operation.

## `git2 recover`

```text
git2 recover [--status | --rollback]
```

Operates on `.git/git2/pending/` even when an interrupted mutation leaves the working tree or Git metadata in a partial layout.

Modes:

- `--status`: read-only inspection of any active pending transaction (`tx_id`, phase, command, before/target views, progress, and deterministic operation ID), leaving sidecar state untouched;
- `--rollback`: rolls back an un-selected (`Prepared`, `Applying`, or `Finalizing`) transaction to its durable `before` view by verifying the pre-state (`Prepared`) or reversing applied actions in reverse order up to `next_action`, refusing after recovery `HEAD` has already moved;
- default (no flags): idempotently reconciles any interrupted transaction—aborting cleanly if `Prepared`, resuming and finalizing if `Applying`/`Finalizing`, or cleaning pending state without duplicating history if recovery `HEAD` already selected the deterministic operation ID. Foreign Git locks, live divergence, or sidecar corruption are refused untouched.

## `git2 oplog`

```text
git2 oplog
```

Requires an adopted repository. When no transaction is pending, it imports external changes if needed (retaining the current Git object closure, like `status` and `checkpoint`), then prints immutable operations newest first. When a pending transaction exists, it prints a warning to standard error, skips external-change import, and displays the last durable history read-only.

Current output fields:

```text
<64-char-operation-id> <unix-seconds> <kind> <64-char-before-view> -> <64-char-after-view>
```

Kinds currently include:

- `adopt`;
- `external-git`;
- `git2:checkpoint`, emitted by `git2 checkpoint`;
- `git2:restore --view <64-hex-view-id>`, emitted by `git2 restore --view <id>`;
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
git2 reset
git2 undo
git2 add
git2 commit
git2 log
```

They will appear only after their transaction, interoperability, and failure-injection exit criteria pass.
