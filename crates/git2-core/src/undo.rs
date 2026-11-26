//! Non-destructive `git2 undo` with semantic index comparison.
//!
//! `undo` restores the `before` view of the latest eligible completed `git2`
//! mutation by appending a new `git2:undo` operation (undoing an undo redoes
//! the mutation). Plain read-only Git commands such as `git status` that only
//! refresh index stat-cache fields or `TREE` extensions do not block `undo`,
//! whereas any staged entry path, stage, mode, OID, flag, worktree, ref, or
//! HEAD divergence is refused.

use std::path::Path;

use bstr::BString;
use gix::index::entry::Flags;

use crate::paths::CheckedRoot;
use crate::recovery::{ContentId, MAX_CAS_BYTES, OperationKind, RecoveryStore, View};
use crate::repository::{LocalRepository, decode_index, validate_index};
use crate::restore;
use crate::transaction::{Failpoint, MutationPlan};
use crate::{Error, Result};

/// Summary returned after a durable `git2 undo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    pub operation_id: ContentId,
    pub restored_view: ContentId,
}

/// Plan a read-only preview for `git2 undo` without modifying Git or sidecar state.
///
/// # Errors
///
/// Returns an error if a pending transaction exists, the latest eligible operation is
/// not undoable (e.g. `adopt`, `checkpoint`, or an outside Git mutation), or the live
/// state diverged semantically from that operation's recorded `after` view.
pub fn plan(repo: &LocalRepository) -> Result<MutationPlan> {
    let store = RecoveryStore::open(repo.git_dir())?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }
    let policy = store.policy()?;
    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index_bytes =
        CheckedRoot::open(repo.git_dir())?.read_file_raw(b"index", MAX_CAS_BYTES as u64)?;
    let target_view = resolve_undo_target(
        &store,
        live_view_id,
        &live_view,
        live_index_bytes.as_deref(),
    )?;
    restore::plan_with_command(repo, target_view, BString::from("undo"))
}

/// Undo the latest eligible completed `git2` operation cleanly without failure injection.
///
/// # Errors
///
/// Returns an error if planning fails, concurrent state diverges, a foreign Git lock exists,
/// or durable execution fails.
pub fn apply(repo: &LocalRepository) -> Result<UndoOutcome> {
    apply_with_hook(repo, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_with_hook(
    repo: &LocalRepository,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<UndoOutcome> {
    let store = RecoveryStore::open(repo.git_dir())?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }
    repo.import_external_changes()?;
    let policy = store.policy()?;
    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index_bytes =
        CheckedRoot::open(repo.git_dir())?.read_file_raw(b"index", MAX_CAS_BYTES as u64)?;
    let restored_view = resolve_undo_target(
        &store,
        live_view_id,
        &live_view,
        live_index_bytes.as_deref(),
    )?;
    let operation_id =
        restore::apply_with_command_and_hook(repo, restored_view, BString::from("undo"), hook)?;
    Ok(UndoOutcome {
        operation_id,
        restored_view,
    })
}

fn resolve_undo_target(
    store: &RecoveryStore,
    live_view_id: ContentId,
    live_view: &View,
    live_index_bytes: Option<&[u8]>,
) -> Result<ContentId> {
    let mut cursor = store.head()?;
    loop {
        let operation = store.read_operation(cursor)?;
        match &operation.kind {
            OperationKind::ExternalGit => {
                let before_view = store.read_view(operation.before)?;
                let after_view = store.read_view(operation.after)?;
                if views_semantically_equal(
                    store,
                    operation.before,
                    &before_view,
                    None,
                    operation.after,
                    &after_view,
                    None,
                )? {
                    let Some(parent) = operation.parent else {
                        return Err(Error::UnsupportedRepository(format!(
                            "latest operation {} has no undoable git2 parent; use `git2 restore --view {}`",
                            cursor, operation.before
                        )));
                    };
                    cursor = parent;
                    continue;
                }
                return Err(Error::UnsupportedRepository(format!(
                    "latest history entry {} is an outside change (`external-git`); use `git2 restore --view {}` to restore the state before it",
                    cursor, operation.before
                )));
            }
            OperationKind::Adopt => {
                return Err(Error::UnsupportedRepository(
                    "cannot undo initial repository adoption".to_owned(),
                ));
            }
            OperationKind::Git2(command) if command.as_slice() == b"checkpoint" => {
                return Err(Error::UnsupportedRepository(format!(
                    "latest operation {} is a checkpoint snapshot, not a mutation; use `git2 restore --view {}`",
                    cursor, operation.before
                )));
            }
            OperationKind::Git2(_) => {
                let expected_after = store.read_view(operation.after)?;
                if !views_semantically_equal(
                    store,
                    live_view_id,
                    live_view,
                    live_index_bytes,
                    operation.after,
                    &expected_after,
                    None,
                )? {
                    return Err(Error::UnsupportedRepository(format!(
                        "current repository state diverged from operation {}'s recorded after-view {}; use `git2 checkpoint` or `git2 restore --view {}` explicitly",
                        cursor, operation.after, operation.before
                    )));
                }
                return Ok(operation.before);
            }
        }
    }
}

/// Compare two views semantically, allowing raw index stat-cache differences while
/// requiring identical `HEAD`, refs, captured files, and staged index entry tuples
/// (path, stage, mode, OID, and semantic flags).
pub(crate) fn views_semantically_equal(
    store: &RecoveryStore,
    left_id: ContentId,
    left: &View,
    left_index_override: Option<&[u8]>,
    right_id: ContentId,
    right: &View,
    right_index_override: Option<&[u8]>,
) -> Result<bool> {
    if left_id == right_id {
        return Ok(true);
    }
    if left.head != right.head || left.refs != right.refs || left.files != right.files {
        return Ok(false);
    }
    match (left.index, right.index) {
        (None, None) => Ok(true),
        (Some(_), None) | (None, Some(_)) => Ok(false),
        (Some(left_idx_id), Some(right_idx_id)) if left_idx_id == right_idx_id => Ok(true),
        (Some(left_idx_id), Some(right_idx_id)) => {
            let left_bytes = load_index_bytes(store, left_idx_id, left_index_override)?;
            let right_bytes = load_index_bytes(store, right_idx_id, right_index_override)?;
            let left_semantic = semantic_index_entries(&left_bytes)?;
            let right_semantic = semantic_index_entries(&right_bytes)?;
            Ok(left_semantic == right_semantic)
        }
    }
}

fn load_index_bytes(
    store: &RecoveryStore,
    id: ContentId,
    override_bytes: Option<&[u8]>,
) -> Result<Vec<u8>> {
    if let Some(bytes) = override_bytes
        && ContentId::for_bytes(bytes) == id
    {
        return Ok(bytes.to_vec());
    }
    store.read_content_bounded(id, MAX_CAS_BYTES)
}

#[derive(PartialEq, Eq)]
struct SemanticIndexEntry {
    path: Vec<u8>,
    stage: u32,
    mode: gix::index::entry::Mode,
    id: gix::ObjectId,
    flags: Flags,
}

fn semantic_index_entries(bytes: &[u8]) -> Result<Vec<SemanticIndexEntry>> {
    let state = decode_index(bytes)?;
    validate_index(&state)?;
    let mask =
        Flags::STAGE_MASK | Flags::ASSUME_VALID | Flags::INTENT_TO_ADD | Flags::SKIP_WORKTREE;
    Ok(state
        .entries()
        .iter()
        .map(|entry| SemanticIndexEntry {
            path: entry.path(&state).to_vec(),
            stage: entry.stage_raw(),
            mode: entry.mode,
            id: entry.id,
            flags: entry.flags & mask,
        })
        .collect())
}

/// Compare two raw views directly without requiring a `LocalRepository` on callers that hold `git_dir`.
#[allow(dead_code)]
pub(crate) fn live_matches_after_semantically(
    git_dir: &Path,
    store: &RecoveryStore,
    live_id: ContentId,
    live_view: &View,
    expected_id: ContentId,
    expected_view: &View,
) -> Result<bool> {
    let live_index_bytes =
        CheckedRoot::open(git_dir)?.read_file_raw(b"index", MAX_CAS_BYTES as u64)?;
    views_semantically_equal(
        store,
        live_id,
        live_view,
        live_index_bytes.as_deref(),
        expected_id,
        expected_view,
        None,
    )
}
