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

## Open decisions

These must be resolved by their roadmap milestones:

- exact pending-transaction binary encoding and phase model;
- consistency/retry policy for concurrent ordinary Git activity;
- reflog requirements for v0.1 commits/resets;
- revision syntax supported by reset/log;
- operation abbreviation and ambiguity rules;
- divergence policy for undoing an older/non-selected operation;
- redo user interface;
- machine/power-loss reliability testing under the strict sidecar sync policy;
- explicit recovery garbage-collection UX;
- whether macOS joins v0.1 or remains a v0.2 target after native testing.
