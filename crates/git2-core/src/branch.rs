//! Native `git2 branch` listing, creation, deletion, and renaming built on the pending journal.

use std::collections::BTreeMap;

use bstr::BString;

use crate::commit;
use crate::object::GitOid;
use crate::paths::CheckedRoot;
use crate::recovery::{ContentId, RecoveryStore, View};
use crate::refs;
use crate::repository::{LocalRepository, known_cas_for_view};
use crate::reset::{rebuild_refs, trim_newline};
use crate::restore::{decode_view_index_readonly, ensure_no_foreign_locks, split_refs};
use crate::retention::MAX_OBJECT_BYTES;
use crate::store::ObjectStore;
use crate::transaction::{self, Action, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

/// One local branch entry returned by [`list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchEntry {
    pub name: BString,
    pub oid: GitOid,
    pub is_current: bool,
    pub subject: BString,
}

/// Summary of local branches and optional detached `HEAD` state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchList {
    pub detached_head: Option<(GitOid, BString)>,
    pub branches: Vec<BranchEntry>,
}

/// Summary returned after a durable `git2 branch` mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchOutcome {
    pub operation_id: ContentId,
    pub after_view: ContentId,
    pub target_commit: GitOid,
    pub changed: bool,
}

/// List local branches from `refs/heads/` and `packed-refs`, marking the checked-out branch.
///
/// # Errors
///
/// Returns an error if `HEAD` or ref files are missing/corrupt or commit objects cannot be read.
pub fn list(repo: &LocalRepository) -> Result<BranchList> {
    let meta = CheckedRoot::open(repo.git_dir())?;
    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;

    let raw_head = meta
        .read_file_raw(b"HEAD", crate::recovery::MAX_CAS_BYTES as u64)?
        .ok_or_else(|| Error::UnsupportedRepository("Git HEAD is missing".to_owned()))?;
    let trimmed_head = trim_newline(&raw_head);
    let (current_branch, detached_head) =
        if let Some(target_ref) = trimmed_head.strip_prefix(b"ref: ") {
            let (leaf, _) = crate::reset::resolve_ref_chain(&meta, target_ref)?;
            let branch = leaf
                .as_slice()
                .strip_prefix(b"refs/heads/")
                .map(BString::from);
            (branch, None)
        } else {
            let oid = GitOid::from_hex(trimmed_head)?;
            let subject = refs::commit_subject(&mut reader, oid)?;
            (None, Some((oid, subject)))
        };

    let raw_branches = refs::read_refs_with_prefix(&meta, b"refs/heads/")?;
    let mut branches = Vec::with_capacity(raw_branches.len());
    for (name, oid) in raw_branches {
        let is_current = current_branch.as_ref() == Some(&name);
        let subject = refs::commit_subject(&mut reader, oid)?;
        branches.push(BranchEntry {
            name,
            oid,
            is_current,
            subject,
        });
    }

    Ok(BranchList {
        detached_head,
        branches,
    })
}

/// Create a new local branch `<name>` at `start_point` (or `HEAD`) without switching to it.
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, `<name>` is
/// invalid/already exists/conflicts with an existing ref, `start_point` cannot resolve to a
/// commit, a foreign Git lock exists, or durable execution fails.
pub fn create(
    repo: &LocalRepository,
    name: &str,
    start_point: Option<&str>,
) -> Result<BranchOutcome> {
    create_with_hook(repo, name, start_point, |_| Ok(()))
}

#[doc(hidden)]
pub fn create_with_hook(
    repo: &LocalRepository,
    name: &str,
    start_point: Option<&str>,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<BranchOutcome> {
    let full_ref = refs::validate_short_ref_name("branch", name, "refs/heads/")?;

    let store = RecoveryStore::open(repo.git_dir())?;
    let lock = store.try_lock()?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }

    repo.import_external_changes_locked(&store, &lock)?;
    let expected_head = store.head()?;
    let policy = store.policy()?;
    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index = decode_view_index_readonly(repo.git_dir(), &live_view)?;

    let lookup = refs::lookup_ref_in_view(&live_view, full_ref.as_slice())?;
    if lookup.resolved_oid.is_some() || lookup.loose_bytes.is_some() {
        return Err(Error::UnsupportedRepository(format!(
            "branch {name:?} already exists"
        )));
    }
    let existing_names = refs::collect_view_ref_names(&live_view)?;
    refs::check_ref_df_conflict(&existing_names, full_ref.as_slice(), None)?;

    let rev = start_point.unwrap_or("HEAD");
    let meta = CheckedRoot::open(repo.git_dir())?;
    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let target_commit = refs::resolve_revision_oid(&meta, &mut reader, rev, true)?;

    let oid_line = BString::from(format!("{target_commit}\n"));
    let (packed, mut loose) = split_refs(&live_view.refs);
    loose.insert(full_ref.clone(), oid_line.clone());
    let new_refs = rebuild_refs(packed, loose);

    let actions = vec![Action::WriteRef {
        name: full_ref,
        expected_before: None,
        target: oid_line,
    }];
    ensure_no_foreign_locks(repo.git_dir(), &actions)?;

    let known_cas = known_cas_for_view(&live_view);
    let target_view_id = commit::persist_commit_view(
        &store,
        &objects,
        &known_cas,
        &live_view,
        live_index.as_ref(),
        live_view.head.clone(),
        new_refs,
        &BTreeMap::new(),
    )?;

    let command = match start_point {
        Some(start) => format!("branch {name} {start}"),
        None => format!("branch {name}"),
    };
    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from(command),
        changes: MutationPlan::changes_for_actions(&actions),
        actions,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(BranchOutcome {
        operation_id,
        after_view: target_view_id,
        target_commit,
        changed: true,
    })
}

/// Delete local branch `<name>` (`force = false` requires it to be merged into `HEAD`).
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, `<name>` is
/// checked out, referenced by a symbolic ref, missing, not merged (when `force = false`),
/// blocked by a foreign lock, or fails durable execution.
pub fn delete(repo: &LocalRepository, name: &str, force: bool) -> Result<BranchOutcome> {
    delete_with_hook(repo, name, force, |_| Ok(()))
}

#[doc(hidden)]
pub fn delete_with_hook(
    repo: &LocalRepository,
    name: &str,
    force: bool,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<BranchOutcome> {
    let full_ref = refs::validate_short_ref_name("branch", name, "refs/heads/")?;

    let store = RecoveryStore::open(repo.git_dir())?;
    let lock = store.try_lock()?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }

    repo.import_external_changes_locked(&store, &lock)?;
    let expected_head = store.head()?;
    let policy = store.policy()?;
    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index = decode_view_index_readonly(repo.git_dir(), &live_view)?;

    if refs::view_head_target_ref(&live_view)?.as_ref() == Some(&full_ref) {
        return Err(Error::UnsupportedRepository(format!(
            "cannot delete branch {name:?} currently checked out at HEAD"
        )));
    }
    let callers = refs::find_loose_symrefs_targeting(&live_view, full_ref.as_slice());
    if let Some(caller) = callers.first() {
        return Err(Error::UnsupportedRepository(format!(
            "cannot delete branch {name:?} referenced by symbolic ref {}",
            escape_path_bytes(caller.as_slice())
        )));
    }

    let deleted = refs::plan_ref_deletion(&live_view, full_ref.as_slice())?;
    let (new_refs, actions, branch_oid) = (deleted.refs, deleted.actions, deleted.resolved_oid);
    let objects = ObjectStore::open(repo.git_dir())?;
    if !force {
        let meta = CheckedRoot::open(repo.git_dir())?;
        let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
        let head_oid = refs::resolve_revision_oid(&meta, &mut reader, "HEAD", true)?;
        let branch_commit = refs::peel_to_commit(&mut reader, branch_oid)?;
        if !refs::is_ancestor(&mut reader, branch_commit, head_oid)? {
            return Err(Error::UnsupportedRepository(format!(
                "branch {name:?} is not fully merged into HEAD; run `git2 branch -D {name}` to force delete"
            )));
        }
    }

    ensure_no_foreign_locks(repo.git_dir(), &actions)?;
    let known_cas = known_cas_for_view(&live_view);
    let target_view_id = commit::persist_commit_view(
        &store,
        &objects,
        &known_cas,
        &live_view,
        live_index.as_ref(),
        live_view.head.clone(),
        new_refs,
        &BTreeMap::new(),
    )?;

    let flag = if force { "-D" } else { "-d" };
    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from(format!("branch {flag} {name}")),
        changes: MutationPlan::changes_for_actions(&actions),
        actions,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(BranchOutcome {
        operation_id,
        after_view: target_view_id,
        target_commit: branch_oid,
        changed: true,
    })
}

/// Rename `old_name` (or the current branch if `None`) to `new_name`.
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, `HEAD` is
/// detached when `old_name` is `None`, the source branch does not exist, `new_name` is
/// invalid/already exists/conflicts, another symbolic ref targets the old branch, a foreign Git
/// lock exists, or durable execution fails.
pub fn rename(
    repo: &LocalRepository,
    old_name: Option<&str>,
    new_name: &str,
) -> Result<BranchOutcome> {
    rename_with_hook(repo, old_name, new_name, |_| Ok(()))
}

#[doc(hidden)]
pub fn rename_with_hook(
    repo: &LocalRepository,
    old_name: Option<&str>,
    new_name: &str,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<BranchOutcome> {
    let store = RecoveryStore::open(repo.git_dir())?;
    let lock = store.try_lock()?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }

    repo.import_external_changes_locked(&store, &lock)?;
    let expected_head = store.head()?;
    let policy = store.policy()?;
    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index = decode_view_index_readonly(repo.git_dir(), &live_view)?;

    let old_short = resolve_rename_source_name(&live_view, old_name)?;
    let old_full = refs::validate_short_ref_name("branch", &old_short, "refs/heads/")?;
    let new_full = refs::validate_short_ref_name("branch", new_name, "refs/heads/")?;

    let deleted = refs::plan_ref_deletion(&live_view, old_full.as_slice())?;
    let (new_refs, mut actions, old_oid) = (deleted.refs, deleted.actions, deleted.resolved_oid);
    if old_full == new_full {
        return Ok(BranchOutcome {
            operation_id: expected_head,
            after_view: live_view_id,
            target_commit: old_oid,
            changed: false,
        });
    }

    let lookup_new = refs::lookup_ref_in_view(&live_view, new_full.as_slice())?;
    if lookup_new.resolved_oid.is_some() || lookup_new.loose_bytes.is_some() {
        return Err(Error::UnsupportedRepository(format!(
            "branch {new_name:?} already exists"
        )));
    }
    let existing_names = refs::collect_view_ref_names(&live_view)?;
    refs::check_ref_df_conflict(
        &existing_names,
        new_full.as_slice(),
        Some(old_full.as_slice()),
    )?;

    let callers = refs::find_loose_symrefs_targeting(&live_view, old_full.as_slice());
    if let Some(caller) = callers.first() {
        return Err(Error::UnsupportedRepository(format!(
            "cannot rename branch {old_short:?} referenced by symbolic ref {}",
            escape_path_bytes(caller.as_slice())
        )));
    }

    let (packed, mut loose) = split_refs(&new_refs);
    let oid_line = BString::from(format!("{old_oid}\n"));
    loose.insert(new_full.clone(), oid_line.clone());
    let final_refs = rebuild_refs(packed, loose);
    actions.push(Action::WriteRef {
        name: new_full.clone(),
        expected_before: None,
        target: oid_line,
    });

    let new_head =
        if refs::direct_head_symref(live_view.head.as_slice()).as_ref() == Some(&old_full) {
            let target = BString::from(format!(
                "ref: {}\n",
                String::from_utf8_lossy(new_full.as_slice())
            ));
            actions.push(Action::WriteHead {
                expected_before: live_view.head.clone(),
                target: target.clone(),
            });
            target
        } else {
            live_view.head.clone()
        };

    ensure_no_foreign_locks(repo.git_dir(), &actions)?;
    let objects = ObjectStore::open(repo.git_dir())?;
    let known_cas = known_cas_for_view(&live_view);
    let target_view_id = commit::persist_commit_view(
        &store,
        &objects,
        &known_cas,
        &live_view,
        live_index.as_ref(),
        new_head,
        final_refs,
        &BTreeMap::new(),
    )?;

    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from(format!("branch -m {old_short} {new_name}")),
        changes: MutationPlan::changes_for_actions(&actions),
        actions,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(BranchOutcome {
        operation_id,
        after_view: target_view_id,
        target_commit: old_oid,
        changed: true,
    })
}

fn resolve_rename_source_name(live_view: &View, old_name: Option<&str>) -> Result<String> {
    if let Some(old) = old_name {
        return Ok(old.to_owned());
    }
    let Some(head_ref) = refs::direct_head_symref(live_view.head.as_slice()) else {
        return Err(Error::UnsupportedRepository(
            "cannot rename current branch while HEAD is detached".to_owned(),
        ));
    };
    let Some(short) = head_ref.as_slice().strip_prefix(b"refs/heads/") else {
        return Err(Error::UnsupportedRepository(format!(
            "HEAD points outside refs/heads/ ({})",
            escape_path_bytes(head_ref.as_slice())
        )));
    };
    Ok(String::from_utf8_lossy(short).into_owned())
}
