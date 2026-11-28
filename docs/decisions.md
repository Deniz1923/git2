# Architecture decisions

This is a lightweight decision log. Entries describe durable choices that shape implementation and roadmap work. A changed decision should be amended with a new entry rather than silently rewriting history after the project has releases.

## Decision index

| ID | Decision | Status |
| --- | --- | --- |
| D001 | Stay compatible with ordinary Git repositories | Accepted |
| D002 | Make local recoverability the first differentiator | Accepted |
| D003 | Store recovery data below `.git/git2/`, never refs | Accepted |
| D004 | Use immutable SHA-256 CAS/views/operations | Accepted |
| D005 | Begin with core + CLI only | Accepted |
| D006 | Own local semantics and use `gix` for narrow plumbing | Accepted |
| D007 | Target Linux, SHA-1, single-worktree repositories first | Accepted |
| D008 | Preserve Git-style staging | Accepted |
| D009 | Capture non-ignored untracked files | Accepted |
| D010 | Import ordinary Git changes into the operation log | Accepted |
| D011 | Undo appends history; it never deletes history | Accepted |
| D012 | Refuse unsupported states rather than degrade silently | Accepted |
| D013 | Defer networking and initially delegate it to Git | Accepted |
| D014 | Do not silently prune recovery data | Accepted |
| D015 | Refuse legacy recovery stores and enforce private v2 storage | Accepted |
| D016 | Compress recovery CAS payloads with zlib in format 3 | Accepted |
| D017 | Sequence recovery proof before native add/commit, constrain journal, and check semantic index for undo | Accepted |
| D018 | Track delegated remote/network commands with `pending/delegated`, conflict-safe rollback, and non-undoable push | Accepted |
| D019 | Separate `switch` for branch/HEAD checkout from `restore` without an overloaded `checkout` command | Accepted |

## D001 — ordinary Git remains the repository format

**Context:** A new VCS format would turn interoperability, hosting, migration, and tooling into the primary project instead of local recovery.

**Decision:** git2 reads and writes standard Git objects, refs, and index data. Git is the compatibility oracle.

**Consequences:** Git tooling and hosting remain usable. git2 must match subtle Git semantics and cannot simplify them by changing the public format.

## D002 — recoverability is the first differentiator

**Context:** Reimplementing Git commands alone has learning value but weak product value. Branching-model innovation would overlap mature systems and dramatically increase scope.

**Decision:** The first product proof is a complete, non-destructive local undo of a supported destructive operation.

**Consequences:** Pending transactions, view restoration, and failure injection outrank networking and broad command coverage.

## D003 — recovery metadata is a private sidecar

**Context:** Storing operation history under `refs/` risks accidental publication, especially through mirror pushes.

**Decision:** Store recovery data only below `.git/git2/`. Never create custom recovery refs.

**Consequences:** Recovery history is clone-local and not transferred by ordinary push/clone. Copying only the worktree or cloning elsewhere loses it. Backing it up requires including the Git directory.

## D004 — immutable SHA-256 recovery graph

**Context:** Recovery data needs deduplication, corruption detection, and stable references independent of Git’s SHA-1 object identity.

**Decision:** Hash raw CAS payloads, encoded views, and encoded operations with SHA-256. Keep those records immutable and select history through one mutable recovery HEAD.

**Consequences:** Unchanged bytes deduplicate and corruption is detectable. Garbage collection must be reachability-based. SHA-256 does not provide authentication.

## D005 — two crates until a real boundary appears

**Context:** The original scaffold split layers into crates before useful interfaces existed.

**Decision:** Keep a private core library and one CLI crate. Add crates only for independently useful, replaceable, or security-sensitive boundaries.

**Consequences:** Refactoring is cheaper and architectural claims follow working code. The core may contain several modules without artificial package boundaries.

## D006 — hybrid implementation boundary

**Context:** Implementing every pack, index, ignore, status, worktree, and network detail would delay the product proof. Delegating all semantics would reduce learning value and control over safety.

**Decision:** git2 owns object encoding, recovery semantics, product refusal policy, and transaction behavior. `gix` provides difficult local plumbing such as packed object lookup, index parsing, ignores, and status traversal.

**Consequences:** Dependency APIs influence implementation, but product invariants remain local and testable. Dependency behavior must still be differentially verified.

## D007 — narrow first compatibility target

**Context:** Linked worktrees, sparse/partial clones, submodules, SHA-256 repositories, and cross-platform filesystems each change what a complete view means.

**Decision:** v0.1 targets ordinary non-bare, single-worktree SHA-1 repositories on Linux and refuses known unsupported features.

**Consequences:** The product can prove one complete guarantee. Broader support requires its own restore/failure tests instead of removing checks.

## D008 — preserve Git-style staging

**Context:** Replacing the index with a new mental model would make interoperability and user expectations harder.

**Decision:** `git2 add`, status columns, commit input, and reset modes will preserve Git’s staging-area semantics.

**Consequences:** Index filters, pathspecs, modes, lockfiles, and conflict stages must be understood and tested. Simpler snapshot-only staging is rejected.

## D009 — include non-ignored untracked content

**Context:** A destructive worktree operation can remove valuable files that are not tracked. Capturing only refs/index/tracked files would make the undo claim incomplete.

**Decision:** Complete supported views include present non-ignored untracked files. Ignored files remain outside v0.1.

**Consequences:** Capture may be storage-heavy and needs file-size limits. Restore must remove/restore untracked paths carefully while preserving ignored files.

## D010 — external Git changes become operations

**Context:** Users will continue using ordinary Git. If git2 ignores those changes, its selected view becomes stale and the operation history is misleading.

**Decision:** Before relevant git2 commands, compare the current supported state with the selected view and append one `external-git` operation when it differs.

**Consequences:** The log remains coherent without replacing Git. There is no background monitoring, so multiple external actions between git2 commands collapse into one imported transition.

## D011 — undo is append-only

**Context:** Moving an operation pointer backward or deleting history makes redo and auditability fragile.

**Decision:** Undo restores an earlier view and appends a new operation with current state as `before` and target state as `after`.

**Consequences:** Undo can itself be undone. History grows until explicit garbage-collection policy exists.

## D012 — refusal beats partial safety

**Context:** A tool centered on recovery cannot silently skip state it does not understand.

**Decision:** Reject unsupported repository states, unsafe paths, corrupt records, special files, and oversized eligible files before mutation where possible.

**Consequences:** Early versions support fewer repositories. Errors and compatibility docs become part of the product interface.

## D013 — networking follows local safety

**Context:** Native Git transport, authentication, negotiation, and hosting compatibility are large independent projects and do not prove local undo.

**Decision:** Defer networking until after v0.1. The first implementation will invoke installed Git using argument vectors and verify that recovery metadata remains private.

**Consequences:** Git is a runtime dependency for future networking. Native protocol work may happen later only with a clear benefit.

## D014 — no silent recovery pruning

**Context:** Automatic pruning could destroy the exact state users expect a recovery tool to retain.

**Decision:** Never silently prune recovery data. Warn about size and later provide explicit reporting/garbage collection.

**Consequences:** Sidecars can grow. A future `git2 gc` must show policy and preserve everything reachable from operation history.

## D015 — owner-private recovery format 2

**Context:** The alpha sidecar did not persist capture limits, could encode data its own reader would reject, and depended on umask alone for privacy.

**Decision:** Use format 2 with an immutable capture policy, symmetric encoder/decoder limits, owner-private access, and strict sync errors. Reserve a versioned map for retained Git objects. Refuse v1 without rewriting it.

**Consequences:** Users of v1 must preserve their old store separately before adopting again. The object map is populated for new captures; views written before retention was implemented remain unchanged. The format 2 capture alpha is still not a restore or backup tool.

## D016 — format 3 zlib-compressed CAS storage

**Context:** Retaining complete Git object closures alongside worktree bytes in uncompressed CAS records inflates `.git/git2/` far beyond Git's compressed loose and packed object storage.

**Decision:** Bump the sidecar format marker to `3\n` and store `.git/git2/cas/` entries RFC 1950 zlib-compressed on disk while keeping `ContentId` as SHA-256 over the uncompressed payload bytes. Enforce bounded decompression, stream-end/trailing-byte checks, and uncompressed SHA-256 verification. Refuse format 1 and format 2 stores byte-for-byte untouched with archive instructions.

**Consequences:** Sidecar disk usage drops substantially without adding a silent garbage collector or complicating `ContentId` deduplication. Pre-release format 2 stores must be archived separately before adopting under format 3.

## D017 — recovery-first build order, constrained journal, and semantic index check for `undo`

**Context:** Building staging and commits before proving crash-safe restore and undo delays validation of git2's core differentiator, an unconstrained transaction journal risks over-engineering, and strict raw-byte `.git/index` checks on `git2 undo` would cause plain read-only `git status` calls to block undo after `git2 reset --hard`.

**Decision:** Sequence v0.1 recovery-first: build the pending-transaction journal (constrained to 4 phases — `Prepared`, `Applying`, `Finalizing`, `Committed` — over a fixed set of primitive checked actions), restore/recover, native `reset`, and append-only `undo`/redo first, pause at an explicit review gate, then build native `add`, `commit`, and `log`. For `git2 undo` divergence checks, ignore stat-cache-only index refreshes while still requiring exact staged paths, stages, modes, OIDs, and flags, and still restoring exact captured raw index bytes.

**Consequences:** Crash-recovery guarantees remain complete with a small, auditable state machine, ordinary `git status` inspection does not break `git2 undo`, and staging/commit complexity stays gated behind the verified recovery proof.

## D018 — delegated pending transactions and conflict-safe network recovery

**Context:** Per D013, `clone`, `fetch`, `pull`, `push`, and `remote` delegate transport to installed `git` via strict argv. Unlike native local commands (`add`, `commit`, `reset`), `fetch` and `pull` cannot compute exact target views beforehand, and `pull --merge` / `pull --rebase` can halt midway with `MERGE_HEAD`, `rebase-merge/`, or multi-stage index conflicts. At the same time, pushing commits to a remote cannot be retracted locally.

**Decision:** Persist a separate format-3 `.git/git2/pending/delegated` record before delegating mutable remote operations (`fetch`, `pull`, `push`, `remote remove`, `remote rename`). Redact embedded URL credentials (`https://user:pass@host`) before persisting command text. Clean up failed `--ff-only` pulls that leave zero conflicts cleanly (recording any fetched tracking-ref changes as `external-git`). On conflicted `pull --merge`/`--rebase` or process kills, keep `pending/delegated` so `git2 recover --rollback` removes transient Git conflict state and restores `before_view`, while `git2 recover` (stripping optional `REUC` index extensions left by manual Git conflict resolution) finalizes resolved work as an undoable `oplog` entry. Refuse `git2 undo` on `git2:push` operations explicitly.

**Consequences:** Format `3\n` sidecar schemas remain backward-compatible, local commands keep zero runtime dependency on `git`, and users get conflict-safe `pull` recovery plus honest push warnings without reimplementing Git wire protocols.

## D019 — dedicated `switch` instead of an overloaded `checkout` command

**Context:** Git's legacy `git checkout` mixes branch switching, detaching `HEAD`, and path-level index/worktree restoration into one overloaded interface where small argument typos can silently discard uncommitted file edits. Meanwhile, `git2 restore --view <id>` already owns snapshot restoration and will expand in Milestone 12 to path-scoped `restore`.

**Decision:** Implement `git2 switch` (`<branch>`, `-c`/`-C <new-branch>`, `--detach`) cleanly for branch and `HEAD` transitions with strict dirty-worktree overwrite protection, rather than adding an overloaded `checkout` command.

**Consequences:** Command responsibilities stay unambiguous (`branch`/`switch`/`tag` for refs and active branch state, `restore` for snapshot and path restoration), reducing accidental user destruction while matching modern Git conventions.

## Open decisions

These must be resolved in upcoming roadmap milestones (`v0.2–v1.0`):

- consistency/retry policy for concurrent ordinary `git` activity during capture and mutation (`v0.3` M16);
- short op/view ID prefix minimum length and collision refusal behavior (`v0.3` M14);
- policy for non-HEAD divergence `undo` vs explicit target-view `restore` (`v0.3` M14);
- `git2 redo` UX and `oplog` filtering/formatting (`v0.3` M14);
- explicit `git2 gc` / `git2 storage` retention defaults (`v0.3` M15);
- power-loss filesystem durability testing harness under strict sidecar sync rules (`v1.0` M22);
- macOS (APFS case-insensitivity/Unicode normalization) and Windows (NTFS handles/ACLs/symlinks) recovery semantics (`v0.5` M20);
- whether `v1.0` retains `git` subprocess delegation for wire transport or adopts native protocol libraries (`v1.0` M22).
