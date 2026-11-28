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
- `git2:reset --<mode> <rev>`, emitted by `git2 reset`;
- `git2:add ...`, emitted by `git2 add`;
- `git2:commit`, emitted by `git2 commit`;
- `git2:branch ...`, `git2:switch ...`, `git2:tag ...`, emitted by branch, switch, and tag mutations (all undoable via `git2 undo`);
- `git2:fetch ...`, `git2:pull ...`, `git2:remote remove|rename ...` (undoable via `git2 undo`), with any embedded URL credentials (`scheme://user:pass@host`) redacted as `***`;
- `git2:push ...`, emitted when local tracking refs move during `git2 push` (explicitly refused by `git2 undo`);
- `git2:undo`, emitted by `git2 undo`;
- other `git2:<command>` values in the format, reserved for future mutating commands.

An unchanged `checkpoint` does not append a new operation, so repeated identical saves leave `oplog` unchanged.

Output is intentionally simple and not yet declared a stable porcelain format.

## `git2 add`

```text
git2 add [--all] [--] [<path>...]
```

Requires an adopted repository and stages changes to `.git/index` transactionally (writing newly staged loose blobs before updating `index` via `.git/index.lock` and appending one `git2:add` operation).

Behavior:

- treats every `<path>` argument literally relative to the current directory (use `--` for leading `-` names; brackets, `*`, newlines, and non-UTF-8 bytes match verbatim with no pathspec globs);
- stages new, modified, and deleted entries (including directory trees and directory deletions), or pass `-A` / `--all` across the repository;
- applies built-in `.gitattributes` / `core.autocrlf` / `core.eol` text-to-Git conversions via `gix`, preserves executable bits and raw symlink targets without filtering;
- refuses active external `filter=` clean/process drivers, non-UTF-8 `working-tree-encoding`, ignored explicit targets (no `--force` in v0.1), nested `.git` directories, special entries, and pre-existing foreign `.git/index.lock` files without touching Git state.

## `git2 commit`

```text
git2 commit -m <message>
```

Requires an adopted repository and creates a root or single-parent commit from the stage-zero index snapshot on detached `HEAD` or the leaf branch referenced by `HEAD`, leaving reflogs unchanged.

Behavior:

- builds canonical trees bottom-up using Git's directory-`/` sort order and persists new trees and the commit via the 4-phase pending journal (recording a `git2:commit` operation reversible via `git2 undo`);
- resolves author and committer identity/timestamps from `GIT_AUTHOR_*` / `GIT_COMMITTER_*` and `user.name` / `user.email`, refusing if either is absent or contains forbidden header bytes;
- refuses blank or NUL commit messages, empty/unchanged trees (`HEAD^{tree}` or empty unborn index), `INTENT_TO_ADD` (`git add -N`) or `REUC` index entries, active executable commit hooks (`pre-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`), `commit.gpgSign = true`, non-UTF-8 `i18n.commitEncoding`, and pre-existing foreign locks before mutation.

## `git2 log`

```text
git2 log [-n|--max-count <n>]
```

Reads bounded first-parent commit history starting from `HEAD` (default `50`, maximum `10000`).

Behavior:

- prints commit ID, author, formatted date/offset, and indented message payload byte-safely to standard output;
- refuses pending transactions, counts above `10000`, unborn `HEAD`, parent cycles, or missing/corrupt parent commits rather than silently truncating history.

## `git2 branch`

```text
git2 branch [-v|--verbose]
git2 branch <name> [<start-point>]
git2 branch (-d|--delete | -D) <branch-name>
git2 branch (-m|--move) [<old-branch>] <new-branch>
```

Requires an adopted repository for mutating operations (listing is read-only) and runs every branch creation, rename, and deletion inside the 4-phase pending-transaction journal (`git2 undo`-capable).

Behavior:

- listing reads both loose `refs/heads/` and `.git/packed-refs`, marking the active branch (or `(HEAD detached at <short-id>)`) with `*`, and `-v` appends the 7-character OID and commit subject;
- create resolves `<start-point>` (default `HEAD`, supporting branches, tags with annotated tag peeling, 40-hex commit IDs, and `~N`) and refuses invalid names, duplicates, D/F ref collisions, and foreign ref locks;
- delete handles both loose and packed branches (refusing to delete the branch checked out at `HEAD` or targeted by another symbolic ref; `-d` verifies full ancestry reachability from `HEAD` across merge commits while `-D` deletes unmerged branches cleanly with full object rehydration on `git2 undo`);
- rename updates loose/packed refs, D/F swaps (`a` ↔ `a/b`), and `HEAD` when renaming the current branch.

## `git2 switch`

```text
git2 switch <branch>
git2 switch (-c <new-branch> | -C <new-branch>) [<start-point>]
git2 switch (-d|--detach) [<commit>]
```

Requires an adopted repository and switches branches or detaches `HEAD` transactionally via 2-tree checkout (`source_tree` → `target_tree`) with dirty-worktree overwrite protection.

Behavior:

- preserves staged changes, unstaged edits, and untracked files on paths unchanged between `HEAD` and the target commit;
- refuses cleanly without touching `HEAD`, `.git/index`, or files if any staged modification, unstaged edit, mode change, untracked file, ignored file, or D/F shape collision would be overwritten;
- handles executables, symlinks, non-UTF-8 paths, file↔directory swaps, `.gitignore` transitions, and built-in EOL conversions, recording `git2:switch ...` operations reversible via `git2 undo`.

## `git2 tag`

```text
git2 tag [-l|--list]
git2 tag <tag-name> [<target>]
git2 tag -a -m <message> <tag-name> [<target>]
git2 tag -d <tag-name>
```

Lists tags across `refs/tags/` and `packed-refs` or performs undoable lightweight/annotated tag creation and deletion.

Behavior:

- lightweight tags create `refs/tags/<name>` pointing directly at the resolved commit/object OID;
- `-a -m <message>` creates an unsigned canonical Git tag object (reusing `GIT_COMMITTER_*` / `user.*` identity, refusing `tag.gpgSign`, `tag.forceSignAnnotated`, non-UTF-8 `i18n.commitEncoding`, blank messages, or foreign locks) and retains the tag object privately so `git2 undo` restores deleted tags even after Git object pruning;
- `-d <tag-name>` removes loose or packed tags cleanly (stripping any peeled `^<oid>` line in `packed-refs`).

## `git2 reset`

```text
git2 reset [--soft | --mixed | --hard] [<commit>]
```

Requires an adopted repository and moves `HEAD` (or its current local branch ref when symbolic) to `<commit>`, leaving `ORIG_HEAD` and reflogs unchanged.

Modes:

- `--soft`: moves only `HEAD` or its target branch ref; leaves the index and working tree untouched;
- `--mixed` (default): moves `HEAD` or its branch and rebuilds `.git/index` from the target commit tree; leaves the working tree untouched;
- `--hard`: moves `HEAD` or its branch, rebuilds `.git/index`, and resets tracked working tree paths (overwriting colliding non-ignored untracked files cleanly so `undo` can recover them, preserving unrelated non-ignored untracked and ignored paths, and refusing ignored obstructions or nested repositories before mutation).

Target revisions:

- defaults to `HEAD` when `<commit>` is omitted;
- accepts `HEAD`, full 40-character SHA-1 commit IDs, local branch names (`refs/heads/<name>`), tag names (`refs/tags/<name>`, peeling annotated tags to commits and refusing ambiguous branch+tag names), and optional `<target>~N` suffixes up to `10,000` first-parent steps;
- refuses unborn targets, `^` / `@{...}` / `..` syntax, non-commit objects, and trees containing submodule gitlinks before mutating state.

## `git2 undo`

```text
git2 undo [--dry-run]
```

Non-destructively undoes the latest eligible completed `git2` mutation (`reset`, `restore`, `undo`, and future mutations) by restoring its durable `before` view through the 4-phase pending journal and appending one `git2:undo` operation. Running `git2 undo` again undoes that undo (redo), keeping the full history intact.

Options:

- `--dry-run`: verifies eligibility and displays single-line byte-safe preview changes without modifying Git state or sidecar history.

Behavior:

- plain read-only Git checks such as `git status` that only refresh `.git/index` stat-cache fields or `TREE` extensions (including when imported as a stat-only `external-git` entry by `status` or `oplog`) do not block `undo`, while `undo` still restores the exact captured raw pre-operation `.git/index` bytes;
- refuses any staged entry path, stage, mode, OID, flag, worktree, ref, or `HEAD` divergence, and refuses `adopt`, `checkpoint`, or semantic `external-git` entries at recovery `HEAD` with an explicit `git2 restore --view <id>` suggestion.

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

## Remote & network commands (delegated to `git`)

All remote/network commands invoke installed `git` using strict argument vectors (`-c protocol.ext.allow=never` and `--` argument barriers), never a shell, while rejecting `-`-prefixed values, `ext::` URLs, and `refs/git2/` refspecs. Unlike local commands, these commands require `git` on `PATH`.

### `git2 remote`

```text
git2 remote [-v|--verbose]
git2 remote add <name> <url>
git2 remote remove <name>
git2 remote rename <old> <new>
git2 remote get-url [--push] [--all] <name>
git2 remote set-url [--push] <name> <newurl> [<oldurl>]
```

Manages configured remotes in `.git/config`. On adopted repositories, `remove` and `rename` execute inside a delegated pending transaction so any deleted or renamed `refs/remotes/<name>/*` tracking refs are captured in `oplog` (`.git/config` itself stays outside v0.1 recovery views).

### `git2 clone`

```text
git2 clone [--branch <name>] [--origin <name>] [--no-adopt] [--max-file-mib <MiB>] [--] <url> [<directory>]
```

Clones `<url>` and automatically runs `git2 adopt --max-file-mib <MiB>` (default `100`) unless `--no-adopt` is passed. If the cloned repository uses an unsupported layout (such as submodules or files above the limit), the valid Git clone remains intact on disk while adoption explains why recovery storage was not created.

### `git2 fetch`

```text
git2 fetch [--all] [-p|--prune] [-t|--tags] [--] [<remote>] [<refspec>...]
```

Requires an adopted repository and delegates `git fetch` inside a delegated pending transaction. Updated tracking refs and tags are recorded as an undoable `git2:fetch` operation; an up-to-date fetch appends no operation, and clean offline/auth failures clear pending state automatically.

### `git2 pull`

```text
git2 pull [--ff-only | --merge | --rebase] [-p|--prune] [--] [<remote>] [<refspec>...]
```

Requires an adopted repository and defaults to `--ff-only`. Optional `--merge` (passes `--no-rebase --no-edit`) and `--rebase` wrap the pull in a conflict-recoverable delegated transaction:

- clean updates append an undoable `git2:pull` operation;
- non-fast-forward `--ff-only` refusals cleanly record any fetched tracking refs without leaving pending state stuck;
- merge or rebase conflicts (or interruptions) keep `.git/git2/pending/delegated` active so you can either resolve conflicts manually in Git and run `git2 recover` (stripping optional `REUC` resolve-undo extensions cleanly) or abort everything back to the pre-pull snapshot with `git2 recover --rollback`.

### `git2 push`

```text
git2 push [-u|--set-upstream] [--tags] [-d|--delete] [--force-with-lease | -f|--force] [-n|--dry-run] [--] [<remote>] [<refspec>...]
```

Refuses any pending transaction or refspec naming `refs/git2/`. Prints a clear notice on standard error (and stronger warning on `--force`) because commits already sent to a remote cannot be retracted by local `git2 undo`. On adopted repositories, any local tracking refs moved by the push record a `git2:push` operation explicitly marked non-undoable by `git2 undo`.
