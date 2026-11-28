//! Native `git2 tag` listing, lightweight tags, annotated tag objects, and deletion.

use std::collections::BTreeMap;

use bstr::BString;

use crate::commit;
use crate::object::{GitOid, ObjectKind, RawObject, Tag};
use crate::paths::CheckedRoot;
use crate::recovery::{ContentId, RecoveryStore};
use crate::refs;
use crate::repository::{LocalRepository, known_cas_for_view};
use crate::reset::rebuild_refs;
use crate::restore::{decode_view_index_readonly, ensure_no_foreign_locks, split_refs};
use crate::retention::MAX_OBJECT_BYTES;
use crate::store::ObjectStore;
use crate::transaction::{self, Action, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

/// One tag entry returned by [`list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagEntry {
    pub name: BString,
    pub oid: GitOid,
}

/// Summary returned after a durable `git2 tag` mutation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagOutcome {
    pub operation_id: ContentId,
    pub after_view: ContentId,
    pub target_oid: GitOid,
    pub tag_oid: GitOid,
}

/// List tags from `refs/tags/` and `packed-refs`, sorted by tag name.
///
/// # Errors
///
/// Returns an error if tag refs or `packed-refs` cannot be read or decoded.
pub fn list(repo: &LocalRepository) -> Result<Vec<TagEntry>> {
    let meta = CheckedRoot::open(repo.git_dir())?;
    let map = refs::read_refs_with_prefix(&meta, b"refs/tags/")?;
    Ok(map
        .into_iter()
        .map(|(name, oid)| TagEntry { name, oid })
        .collect())
}

/// Create a lightweight tag `<name>` pointing directly at `target` (or `HEAD`).
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, `<name>` is
/// invalid/already exists/conflicts with another ref, `target` cannot resolve, a foreign Git
/// lock exists, or durable execution fails.
pub fn create_lightweight(
    repo: &LocalRepository,
    name: &str,
    target: Option<&str>,
) -> Result<TagOutcome> {
    create_lightweight_with_hook(repo, name, target, |_| Ok(()))
}

#[doc(hidden)]
pub fn create_lightweight_with_hook(
    repo: &LocalRepository,
    name: &str,
    target: Option<&str>,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<TagOutcome> {
    let full_ref = refs::validate_short_ref_name("tag", name, "refs/tags/")?;

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
            "tag {name:?} already exists"
        )));
    }
    let existing_names = refs::collect_view_ref_names(&live_view)?;
    refs::check_ref_df_conflict(&existing_names, full_ref.as_slice(), None)?;

    let rev = target.unwrap_or("HEAD");
    let meta = CheckedRoot::open(repo.git_dir())?;
    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let target_oid = refs::resolve_revision_oid(&meta, &mut reader, rev, false)?;

    let oid_line = BString::from(format!("{target_oid}\n"));
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

    let command = match target {
        Some(t) => format!("tag {name} {t}"),
        None => format!("tag {name}"),
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
    Ok(TagOutcome {
        operation_id,
        after_view: target_view_id,
        target_oid,
        tag_oid: target_oid,
    })
}

/// Create an unsigned annotated tag object and `refs/tags/<name>` ref targeting `target` (or `HEAD`).
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, signing config is
/// active (`tag.gpgSign` or `tag.forceSignAnnotated`), the message is blank or contains NUL,
/// identity is missing, `<name>` is invalid/already exists/conflicts, `target` cannot resolve, a
/// foreign lock exists, or durable execution fails.
pub fn create_annotated(
    repo: &LocalRepository,
    name: &str,
    message: &str,
    target: Option<&str>,
) -> Result<TagOutcome> {
    create_annotated_with_hook(repo, name, message, target, |_| Ok(()))
}

#[doc(hidden)]
pub fn create_annotated_with_hook(
    repo: &LocalRepository,
    name: &str,
    message: &str,
    target: Option<&str>,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<TagOutcome> {
    let full_ref = refs::validate_short_ref_name("tag", name, "refs/tags/")?;
    let normalized_message = commit::normalize_message(message)?;

    let store = RecoveryStore::open(repo.git_dir())?;
    let lock = store.try_lock()?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }

    check_annotated_tag_config(repo)?;
    let now = gix::date::Time::now_local_or_utc();
    let tagger = commit::resolve_signature(
        repo,
        "tagger",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_COMMITTER_DATE",
        now,
    )?;

    repo.import_external_changes_locked(&store, &lock)?;
    let expected_head = store.head()?;
    let policy = store.policy()?;
    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index = decode_view_index_readonly(repo.git_dir(), &live_view)?;

    let lookup = refs::lookup_ref_in_view(&live_view, full_ref.as_slice())?;
    if lookup.resolved_oid.is_some() || lookup.loose_bytes.is_some() {
        return Err(Error::UnsupportedRepository(format!(
            "tag {name:?} already exists"
        )));
    }
    let existing_names = refs::collect_view_ref_names(&live_view)?;
    refs::check_ref_df_conflict(&existing_names, full_ref.as_slice(), None)?;

    let rev = target.unwrap_or("HEAD");
    let meta = CheckedRoot::open(repo.git_dir())?;
    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let target_oid = refs::resolve_revision_oid(&meta, &mut reader, rev, false)?;
    let target_obj = reader.read(target_oid)?;

    let tag = Tag {
        object: target_oid,
        object_kind: target_obj.kind,
        name: BString::from(name),
        tagger: Some(tagger),
        extra_headers: Vec::new(),
        message: normalized_message,
    };
    let tag_raw = RawObject {
        kind: ObjectKind::Tag,
        data: tag.serialize(),
    };
    let tag_oid = tag_raw.id();

    let known_cas = known_cas_for_view(&live_view);
    let (pending_objects, mut actions) =
        commit::persist_pending_objects(&store, &mut reader, &known_cas, vec![(tag_oid, tag_raw)])?;

    let oid_line = BString::from(format!("{tag_oid}\n"));
    let (packed, mut loose) = split_refs(&live_view.refs);
    loose.insert(full_ref.clone(), oid_line.clone());
    let new_refs = rebuild_refs(packed, loose);
    actions.push(Action::WriteRef {
        name: full_ref,
        expected_before: None,
        target: oid_line,
    });
    ensure_no_foreign_locks(repo.git_dir(), &actions)?;

    let target_view_id = commit::persist_commit_view(
        &store,
        &objects,
        &known_cas,
        &live_view,
        live_index.as_ref(),
        live_view.head.clone(),
        new_refs,
        &pending_objects,
    )?;

    let command = match target {
        Some(t) => format!("tag -a {name} {t}"),
        None => format!("tag -a {name}"),
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
    Ok(TagOutcome {
        operation_id,
        after_view: target_view_id,
        target_oid,
        tag_oid,
    })
}

fn check_annotated_tag_config(repo: &LocalRepository) -> Result<()> {
    let config = repo.inner.config_snapshot();
    if config.boolean("tag.gpgSign") == Some(true)
        || config.boolean("tag.forceSignAnnotated") == Some(true)
    {
        return Err(Error::UnsupportedRepository(
            "tag signing (`tag.gpgSign` / `tag.forceSignAnnotated`) is not supported".to_owned(),
        ));
    }
    commit::check_commit_encoding(&config)
}

/// Delete tag `<name>` (whether loose or packed).
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, `<name>` is
/// invalid/missing/referenced by a symbolic ref, a foreign lock exists, or durable execution fails.
pub fn delete(repo: &LocalRepository, name: &str) -> Result<TagOutcome> {
    delete_with_hook(repo, name, |_| Ok(()))
}

#[doc(hidden)]
pub fn delete_with_hook(
    repo: &LocalRepository,
    name: &str,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<TagOutcome> {
    let full_ref = refs::validate_short_ref_name("tag", name, "refs/tags/")?;

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

    let callers = refs::find_loose_symrefs_targeting(&live_view, full_ref.as_slice());
    if let Some(caller) = callers.first() {
        return Err(Error::UnsupportedRepository(format!(
            "cannot delete tag {name:?} referenced by symbolic ref {}",
            escape_path_bytes(caller.as_slice())
        )));
    }
    if refs::view_head_target_ref(&live_view)?.as_ref() == Some(&full_ref) {
        return Err(Error::UnsupportedRepository(format!(
            "cannot delete tag {name:?} referenced by HEAD"
        )));
    }

    let deleted = refs::plan_ref_deletion(&live_view, full_ref.as_slice())?;
    let (new_refs, actions, tag_oid) = (deleted.refs, deleted.actions, deleted.resolved_oid);
    ensure_no_foreign_locks(repo.git_dir(), &actions)?;

    let objects = ObjectStore::open(repo.git_dir())?;
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

    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from(format!("tag -d {name}")),
        changes: MutationPlan::changes_for_actions(&actions),
        actions,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(TagOutcome {
        operation_id,
        after_view: target_view_id,
        target_oid: tag_oid,
        tag_oid,
    })
}
