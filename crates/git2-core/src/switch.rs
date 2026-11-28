//! Native `git2 switch` branch/HEAD switching with dirty-worktree overwrite protection.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bstr::{BString, ByteSlice};
use gix::ignore::glob::pattern::Case;
use gix::index::entry::{Flags, Mode, Stat};

use crate::commit;
use crate::object::{Commit, GitOid, ObjectKind, hash_object};
use crate::paths::{CheckedRoot, PathEntryKind};
use crate::recovery::{
    CapturePolicy, ContentId, FileKind, FileRecord, MAX_CAS_BYTES, RecoveryStore, View,
};
use crate::refs;
use crate::repository::{LocalRepository, known_cas_for_view};
use crate::reset::{
    flatten_tree, persist_target_view, rebuild_refs, serialize_index, trim_newline,
};
use crate::restore::{
    IgnoreProfile, MAX_SYMLINK_TARGET_BYTES, ancestor_dir_set, check_nested_git_dir,
    component_count, decode_view_index_readonly, ensure_no_foreign_locks, file_map,
    has_ancestor_in, is_gitignore_path, join_rel, mark_ancestors, push_dir_lifecycle_actions,
    split_refs,
};
use crate::retention::MAX_OBJECT_BYTES;
use crate::stage::RepoFilter;
use crate::store::{BoundedReader, ObjectStore};
use crate::transaction::{self, Action, EntryState, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

/// Target specification for `git2 switch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchTarget {
    /// Switch to an existing local branch (`refs/heads/<name>`).
    Branch(String),
    /// Create (or reset when `force = true`) `<name>` at `start_point` (default `HEAD`) and switch to it.
    Create {
        name: String,
        start_point: Option<String>,
        force: bool,
    },
    /// Detach `HEAD` at `commit` (default `HEAD`).
    Detach { commit: Option<String> },
}

/// Summary returned after a durable `git2 switch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    pub operation_id: ContentId,
    pub after_view: ContentId,
    pub target_commit: GitOid,
    pub detached: bool,
    pub branch: Option<BString>,
    pub changed: bool,
}

#[derive(Clone, Copy)]
struct IndexEntryInfo {
    kind: FileKind,
    mode: Mode,
    oid: GitOid,
    flags: Flags,
    stat: Stat,
    is_intent_to_add: bool,
}

struct TargetPlan {
    target_commit: GitOid,
    new_head: BString,
    new_refs: Vec<(BString, BString)>,
    ref_action: Option<Action>,
    detached: bool,
    branch: Option<BString>,
    command: String,
    no_op: bool,
}

/// Switch branches or detach `HEAD`, preserving clean/non-conflicting local edits and refusing
/// before mutation if any local change or untracked file would be overwritten.
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, the target
/// branch/revision is invalid or missing, local staged/unstaged/untracked/ignored state would be
/// overwritten, a foreign Git lock exists, or durable execution fails.
pub fn apply(repo: &LocalRepository, target: &SwitchTarget) -> Result<SwitchOutcome> {
    apply_with_hook(repo, target, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_with_hook(
    repo: &LocalRepository,
    target: &SwitchTarget,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<SwitchOutcome> {
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

    let meta = CheckedRoot::open(repo.git_dir())?;
    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;

    let resolved = resolve_switch_target(&meta, &mut reader, &live_view, target)?;
    if resolved.no_op {
        return Ok(SwitchOutcome {
            operation_id: expected_head,
            after_view: live_view_id,
            target_commit: resolved.target_commit,
            detached: resolved.detached,
            branch: resolved.branch,
            changed: false,
        });
    }

    let source_tree = read_current_head_tree(&meta, &mut reader, &live_view)?;
    let target_tree = read_commit_tree(&mut reader, resolved.target_commit)?;
    let known_cas = known_cas_for_view(&live_view);

    let (actions, target_view_id) = build_switch_plan(
        repo,
        &store,
        &objects,
        &mut reader,
        &known_cas,
        &source_tree,
        &target_tree,
        &live_view,
        live_index.as_ref(),
        &resolved,
        policy,
    )?;

    if actions.is_empty() && target_view_id == live_view_id {
        return Ok(SwitchOutcome {
            operation_id: expected_head,
            after_view: live_view_id,
            target_commit: resolved.target_commit,
            detached: resolved.detached,
            branch: resolved.branch,
            changed: false,
        });
    }

    let changes = MutationPlan::changes_for_actions(&actions);
    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from(resolved.command),
        actions,
        changes,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(SwitchOutcome {
        operation_id,
        after_view: target_view_id,
        target_commit: resolved.target_commit,
        detached: resolved.detached,
        branch: resolved.branch,
        changed: true,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_switch_plan(
    repo: &LocalRepository,
    store: &RecoveryStore,
    objects: &ObjectStore,
    reader: &mut BoundedReader,
    known_cas: &BTreeSet<ContentId>,
    source_tree: &BTreeMap<BString, (FileKind, GitOid)>,
    target_tree: &BTreeMap<BString, (FileKind, GitOid)>,
    live_view: &View,
    live_index: Option<&gix::index::State>,
    resolved: &TargetPlan,
    policy: CapturePolicy,
) -> Result<(Vec<Action>, ContentId)> {
    if source_tree == target_tree {
        let mut actions = Vec::new();
        if let Some(action) = &resolved.ref_action {
            actions.push(action.clone());
        }
        if live_view.head != resolved.new_head {
            actions.push(Action::WriteHead {
                expected_before: live_view.head.clone(),
                target: resolved.new_head.clone(),
            });
        }
        ensure_no_foreign_locks(repo.git_dir(), &actions)?;
        let view_id = commit::persist_commit_view(
            store,
            objects,
            known_cas,
            live_view,
            live_index,
            resolved.new_head.clone(),
            resolved.new_refs.clone(),
            &BTreeMap::new(),
        )?;
        return Ok((actions, view_id));
    }

    let prepared = prepare_two_tree_checkout(
        repo,
        reader,
        source_tree,
        target_tree,
        live_view,
        live_index,
        policy,
    )?;
    let mut actions = prepared.actions;
    if let Some(action) = &resolved.ref_action {
        actions.push(action.clone());
    }
    if live_view.head != resolved.new_head {
        actions.push(Action::WriteHead {
            expected_before: live_view.head.clone(),
            target: resolved.new_head.clone(),
        });
    }
    ensure_no_foreign_locks(repo.git_dir(), &actions)?;
    let view_id = persist_target_view(
        store,
        objects,
        known_cas,
        resolved.new_head.clone(),
        resolved.new_refs.clone(),
        prepared.target_index_bytes.as_deref(),
        prepared.target_files,
        prepared.cas_writes,
    )?;
    Ok((actions, view_id))
}

fn resolve_switch_target(
    meta: &CheckedRoot,
    reader: &mut BoundedReader,
    live_view: &View,
    target: &SwitchTarget,
) -> Result<TargetPlan> {
    let (packed, mut loose) = split_refs(&live_view.refs);
    match target {
        SwitchTarget::Branch(name) => {
            let full_ref = refs::validate_short_ref_name("branch", name, "refs/heads/")?;
            let lookup = refs::lookup_ref_in_view(live_view, full_ref.as_slice())?;
            let oid = lookup.resolved_oid.ok_or_else(|| {
                Error::UnsupportedRepository(format!(
                    "unknown local branch {name:?}; pass `-c {name}` to create it or `--detach` for tags/commits"
                ))
            })?;
            let target_commit = refs::peel_to_commit(reader, oid)?;
            let new_head = BString::from(format!("ref: refs/heads/{name}\n"));
            let no_op = live_view.head == new_head;
            Ok(TargetPlan {
                target_commit,
                new_head,
                new_refs: rebuild_refs(packed, loose),
                ref_action: None,
                detached: false,
                branch: Some(BString::from(name.as_str())),
                command: format!("switch {name}"),
                no_op,
            })
        }
        SwitchTarget::Create {
            name,
            start_point,
            force,
        } => {
            let full_ref = refs::validate_short_ref_name("branch", name, "refs/heads/")?;
            let lookup = refs::lookup_ref_in_view(live_view, full_ref.as_slice())?;
            if !*force && (lookup.resolved_oid.is_some() || lookup.loose_bytes.is_some()) {
                return Err(Error::UnsupportedRepository(format!(
                    "branch {name:?} already exists; use `-C {name}` to force reset it"
                )));
            }
            let existing_names = refs::collect_view_ref_names(live_view)?;
            refs::check_ref_df_conflict(
                &existing_names,
                full_ref.as_slice(),
                force.then_some(full_ref.as_slice()),
            )?;

            let rev = start_point.as_deref().unwrap_or("HEAD");
            let target_commit = refs::resolve_revision_oid(meta, reader, rev, true)?;
            let oid_line = BString::from(format!("{target_commit}\n"));
            let new_head = BString::from(format!("ref: refs/heads/{name}\n"));

            let ref_action = if lookup.loose_bytes.as_ref() == Some(&oid_line)
                && lookup.resolved_oid == Some(target_commit)
            {
                None
            } else {
                loose.insert(full_ref.clone(), oid_line.clone());
                Some(Action::WriteRef {
                    name: full_ref,
                    expected_before: lookup.loose_bytes,
                    target: oid_line,
                })
            };
            let no_op = ref_action.is_none() && live_view.head == new_head;
            let command = match (force, start_point) {
                (false, None) => format!("switch -c {name}"),
                (false, Some(start)) => format!("switch -c {name} {start}"),
                (true, None) => format!("switch -C {name}"),
                (true, Some(start)) => format!("switch -C {name} {start}"),
            };
            Ok(TargetPlan {
                target_commit,
                new_head,
                new_refs: rebuild_refs(packed, loose),
                ref_action,
                detached: false,
                branch: Some(BString::from(name.as_str())),
                command,
                no_op,
            })
        }
        SwitchTarget::Detach { commit } => {
            let rev = commit.as_deref().unwrap_or("HEAD");
            let target_commit = refs::resolve_revision_oid(meta, reader, rev, true)?;
            let new_head = BString::from(format!("{target_commit}\n"));
            let no_op = live_view.head == new_head;
            let command = match commit {
                Some(rev_arg) => format!("switch --detach {rev_arg}"),
                None => "switch --detach".to_owned(),
            };
            Ok(TargetPlan {
                target_commit,
                new_head,
                new_refs: rebuild_refs(packed, loose),
                ref_action: None,
                detached: true,
                branch: None,
                command,
                no_op,
            })
        }
    }
}

fn read_current_head_tree(
    meta: &CheckedRoot,
    reader: &mut BoundedReader,
    live_view: &View,
) -> Result<BTreeMap<BString, (FileKind, GitOid)>> {
    let trimmed = trim_newline(live_view.head.as_slice());
    let head_commit = if let Some(target_ref) = trimmed.strip_prefix(b"ref: ") {
        let (_, oid) = crate::reset::resolve_ref_chain(meta, target_ref)?;
        match oid {
            Some(oid) => refs::peel_to_commit(reader, oid)?,
            None => return Ok(BTreeMap::new()),
        }
    } else {
        let oid = GitOid::from_hex(trimmed)?;
        refs::peel_to_commit(reader, oid)?
    };
    read_commit_tree(reader, head_commit)
}

fn read_commit_tree(
    reader: &mut BoundedReader,
    commit_oid: GitOid,
) -> Result<BTreeMap<BString, (FileKind, GitOid)>> {
    let obj = reader.read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {commit_oid} is a {}, expected commit",
            obj.kind
        )));
    }
    let commit = Commit::parse(&obj.data)?;
    let mut tree_entries = BTreeMap::new();
    flatten_tree(reader, commit.tree, b"", &mut tree_entries)?;
    Ok(tree_entries)
}

struct PreparedSwitch {
    actions: Vec<Action>,
    target_index_bytes: Option<Vec<u8>>,
    target_files: Vec<FileRecord>,
    cas_writes: Vec<Vec<u8>>,
}

struct TwoTreePlan {
    remove_paths: BTreeSet<BString>,
    write_targets: BTreeMap<BString, (FileKind, GitOid)>,
    new_index_entries: BTreeMap<BString, IndexEntryInfo>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn prepare_two_tree_checkout(
    repo: &LocalRepository,
    reader: &mut BoundedReader,
    source_tree: &BTreeMap<BString, (FileKind, GitOid)>,
    target_tree: &BTreeMap<BString, (FileKind, GitOid)>,
    live_view: &View,
    live_index: Option<&gix::index::State>,
    policy: CapturePolicy,
) -> Result<PreparedSwitch> {
    let empty_index = gix::index::State::new(gix::hash::Kind::Sha1);
    let base_index = live_index.unwrap_or(&empty_index);
    let current_index_map = extract_index_map(base_index)?;
    let worktree = CheckedRoot::open(repo.workdir())?;
    let mut clean_filter = RepoFilter::new_clean(repo, base_index)?;

    let two_tree = plan_two_tree_paths(
        &worktree,
        &mut clean_filter,
        base_index,
        source_tree,
        target_tree,
        &current_index_map,
        policy,
    )?;

    let new_index_bytes = serialize_switch_index(&two_tree.new_index_entries)?;
    let new_index = crate::repository::decode_index(&new_index_bytes)?;
    crate::repository::validate_index(&new_index)?;
    let new_index_id = ContentId::for_bytes(&new_index_bytes);

    let mut cas_writes = Vec::new();
    if live_view.index != Some(new_index_id) {
        cas_writes.push(new_index_bytes.clone());
    }

    let live_map = file_map(live_view);
    let mut smudge_filter = RepoFilter::new_smudge(repo, &new_index)?;
    let (write_blobs, mut blob_cas_writes, mut target_gitignores) = materialize_written_blobs(
        reader,
        &mut smudge_filter,
        &live_map,
        &two_tree.write_targets,
        policy,
    )?;
    cas_writes.append(&mut blob_cas_writes);

    let excludes_file = repo.global_excludes_file()?;
    let case = if repo.ignore_case() {
        Case::Fold
    } else {
        Case::Sensitive
    };
    let current_profile = IgnoreProfile::for_live(
        repo.git_dir(),
        repo.workdir(),
        &worktree,
        excludes_file.clone(),
        case,
        live_index,
        Some(&new_index),
        policy,
    )?;

    let write_file_set: BTreeSet<&[u8]> = write_blobs.keys().map(|p| p.as_slice()).collect();
    let write_dirs = ancestor_dir_set(write_blobs.keys().map(|p| p.as_slice()));
    let kept_tracked: BTreeSet<BString> = two_tree
        .new_index_entries
        .keys()
        .filter(|path| !write_blobs.contains_key(*path))
        .cloned()
        .collect();
    for path in live_map.keys() {
        if !two_tree.remove_paths.contains(path)
            && !write_blobs.contains_key(path)
            && !write_dirs.contains(path)
            && !has_ancestor_in(path.as_slice(), &write_file_set)
            && is_gitignore_path(path.as_slice())
            && let Some(captured) = worktree.read_entry(path.as_slice(), policy.max_file_bytes)?
            && captured.kind != FileKind::Symlink
        {
            target_gitignores.push((path.clone(), captured.bytes));
        }
    }

    let target_profile = IgnoreProfile::from_gitignore_blobs(
        repo.git_dir(),
        repo.workdir(),
        excludes_file,
        case,
        Some(&new_index),
        target_gitignores,
    )?;

    let scan = inspect_switch_worktree(
        &worktree,
        policy,
        &live_map,
        &two_tree.remove_paths,
        &kept_tracked,
        &write_blobs,
        &write_dirs,
        &write_file_set,
        &current_profile,
        &target_profile,
    )?;

    let actions = build_switch_actions(
        &live_map,
        &two_tree.remove_paths,
        &write_blobs,
        &write_dirs,
        &write_file_set,
        live_view.index,
        new_index_id,
        &scan,
    );

    let target_files = build_target_files(
        &live_map,
        &two_tree.remove_paths,
        &kept_tracked,
        &write_blobs,
        &target_profile,
        &scan.newly_exposed_ignored,
        &mut cas_writes,
    );

    Ok(PreparedSwitch {
        actions,
        target_index_bytes: Some(new_index_bytes),
        target_files,
        cas_writes,
    })
}

fn extract_index_map(index: &gix::index::State) -> Result<BTreeMap<BString, IndexEntryInfo>> {
    let mut map = BTreeMap::new();
    for entry in index.entries() {
        let path = BString::from(entry.path(index).as_bytes());
        let kind = match entry.mode {
            Mode::FILE => FileKind::Regular,
            Mode::FILE_EXECUTABLE => FileKind::Executable,
            Mode::SYMLINK => FileKind::Symlink,
            _ => {
                return Err(Error::UnsupportedRepository(format!(
                    "unsupported staged entry mode {:?} for {}",
                    entry.mode,
                    escape_path_bytes(path.as_slice())
                )));
            }
        };
        let oid_bytes: [u8; 20] = entry.id.as_slice().try_into().map_err(|_| {
            Error::CorruptObject("index entry object id has unexpected length".to_owned())
        })?;
        map.insert(
            path,
            IndexEntryInfo {
                kind,
                mode: entry.mode,
                oid: GitOid::from_bytes(oid_bytes),
                flags: entry.flags,
                stat: entry.stat,
                is_intent_to_add: entry.flags.contains(Flags::INTENT_TO_ADD),
            },
        );
    }
    Ok(map)
}

#[allow(clippy::too_many_lines)]
fn plan_two_tree_paths(
    worktree: &CheckedRoot,
    clean_filter: &mut RepoFilter<'_>,
    base_index: &gix::index::State,
    source_tree: &BTreeMap<BString, (FileKind, GitOid)>,
    target_tree: &BTreeMap<BString, (FileKind, GitOid)>,
    current_index: &BTreeMap<BString, IndexEntryInfo>,
    policy: CapturePolicy,
) -> Result<TwoTreePlan> {
    let mut all_paths: BTreeSet<&BString> = BTreeSet::new();
    all_paths.extend(source_tree.keys());
    all_paths.extend(target_tree.keys());

    let mut new_index_entries = current_index.clone();
    let mut remove_paths = BTreeSet::new();
    let mut write_targets = BTreeMap::new();

    for path in all_paths {
        let o = source_tree.get(path).copied();
        let t = target_tree.get(path).copied();
        if o == t {
            continue;
        }
        let i = current_index.get(path).copied();
        match (o, t) {
            (Some(old_entry), None) => match i {
                Some(idx_entry)
                    if !idx_entry.is_intent_to_add
                        && (idx_entry.kind, idx_entry.oid) == old_entry =>
                {
                    if let Some(disk) =
                        worktree.read_entry(path.as_slice(), policy.max_file_bytes)?
                    {
                        if !disk_entry_matches(
                            path.as_slice(),
                            &disk,
                            idx_entry.kind,
                            idx_entry.oid,
                            clean_filter,
                            base_index,
                        )? {
                            return Err(Error::UnsupportedRepository(format!(
                                "local changes to {} would be overwritten by switch",
                                escape_path_bytes(path.as_slice())
                            )));
                        }
                    } else if worktree.entry_kind(path.as_slice())?.is_some() {
                        return Err(Error::UnsupportedRepository(format!(
                            "local entry at {} would be overwritten by switch",
                            escape_path_bytes(path.as_slice())
                        )));
                    }
                    new_index_entries.remove(path);
                    remove_paths.insert(path.clone());
                }
                Some(_) => {
                    return Err(Error::UnsupportedRepository(format!(
                        "local staged changes to {} would be overwritten by switch",
                        escape_path_bytes(path.as_slice())
                    )));
                }
                None => {
                    if matches!(
                        worktree.entry_kind(path.as_slice())?,
                        Some(kind) if kind != PathEntryKind::Directory
                    ) {
                        return Err(Error::UnsupportedRepository(format!(
                            "local changes to {} would be overwritten by switch",
                            escape_path_bytes(path.as_slice())
                        )));
                    }
                }
            },
            (None, Some(new_entry)) => {
                if let Some(idx_entry) = i {
                    if !idx_entry.is_intent_to_add
                        && (idx_entry.kind, idx_entry.oid) == new_entry
                        && let Some(disk) =
                            worktree.read_entry(path.as_slice(), policy.max_file_bytes)?
                        && disk_entry_matches(
                            path.as_slice(),
                            &disk,
                            idx_entry.kind,
                            idx_entry.oid,
                            clean_filter,
                            base_index,
                        )?
                    {
                        continue;
                    }
                    return Err(Error::UnsupportedRepository(format!(
                        "local changes to {} would be overwritten by switch",
                        escape_path_bytes(path.as_slice())
                    )));
                }
                if matches!(
                    optional_entry_kind(worktree, path.as_slice(), &remove_paths)?,
                    Some(kind) if kind != PathEntryKind::Directory
                ) {
                    return Err(Error::UnsupportedRepository(format!(
                        "untracked file {} would be overwritten by switch",
                        escape_path_bytes(path.as_slice())
                    )));
                }
                new_index_entries.insert(path.clone(), new_index_entry(new_entry.0, new_entry.1));
                write_targets.insert(path.clone(), new_entry);
            }
            (Some(old_entry), Some(new_entry)) => {
                if let Some(idx_entry) = i {
                    if !idx_entry.is_intent_to_add
                        && (idx_entry.kind, idx_entry.oid) == new_entry
                        && let Some(disk) =
                            worktree.read_entry(path.as_slice(), policy.max_file_bytes)?
                        && disk_entry_matches(
                            path.as_slice(),
                            &disk,
                            idx_entry.kind,
                            idx_entry.oid,
                            clean_filter,
                            base_index,
                        )?
                    {
                        continue;
                    }
                    if idx_entry.is_intent_to_add || (idx_entry.kind, idx_entry.oid) != old_entry {
                        return Err(Error::UnsupportedRepository(format!(
                            "local staged changes to {} would be overwritten by switch",
                            escape_path_bytes(path.as_slice())
                        )));
                    }
                    let disk = worktree
                        .read_entry(path.as_slice(), policy.max_file_bytes)?
                        .ok_or_else(|| {
                            Error::UnsupportedRepository(format!(
                                "local deletion of {} would be overwritten by switch",
                                escape_path_bytes(path.as_slice())
                            ))
                        })?;
                    if !disk_entry_matches(
                        path.as_slice(),
                        &disk,
                        old_entry.0,
                        old_entry.1,
                        clean_filter,
                        base_index,
                    )? {
                        return Err(Error::UnsupportedRepository(format!(
                            "local changes to {} would be overwritten by switch",
                            escape_path_bytes(path.as_slice())
                        )));
                    }
                    new_index_entries
                        .insert(path.clone(), new_index_entry(new_entry.0, new_entry.1));
                    write_targets.insert(path.clone(), new_entry);
                } else {
                    return Err(Error::UnsupportedRepository(format!(
                        "local deletion of {} would be overwritten by switch",
                        escape_path_bytes(path.as_slice())
                    )));
                }
            }
            (None, None) => {}
        }
    }

    Ok(TwoTreePlan {
        remove_paths,
        write_targets,
        new_index_entries,
    })
}

fn optional_entry_kind(
    worktree: &CheckedRoot,
    path: &[u8],
    remove_paths: &BTreeSet<BString>,
) -> Result<Option<PathEntryKind>> {
    match worktree.entry_kind(path) {
        Ok(kind) => Ok(kind),
        Err(Error::Io(error))
            if error.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error()) =>
        {
            let removed_set: BTreeSet<&[u8]> = remove_paths.iter().map(|p| p.as_slice()).collect();
            if has_ancestor_in(path, &removed_set) {
                Ok(None)
            } else {
                Err(Error::UnsupportedRepository(format!(
                    "local file blocks target directory for {}",
                    escape_path_bytes(path)
                )))
            }
        }
        Err(error) => Err(error),
    }
}

fn new_index_entry(kind: FileKind, oid: GitOid) -> IndexEntryInfo {
    let mode = match kind {
        FileKind::Regular => Mode::FILE,
        FileKind::Executable => Mode::FILE_EXECUTABLE,
        FileKind::Symlink => Mode::SYMLINK,
    };
    IndexEntryInfo {
        kind,
        mode,
        oid,
        flags: Flags::empty(),
        stat: Stat::default(),
        is_intent_to_add: false,
    }
}

fn disk_entry_matches(
    path: &[u8],
    disk: &crate::paths::CapturedEntry,
    expected_kind: FileKind,
    expected_oid: GitOid,
    clean_filter: &mut RepoFilter<'_>,
    base_index: &gix::index::State,
) -> Result<bool> {
    if disk.kind != expected_kind {
        return Ok(false);
    }
    let git_bytes = if disk.kind == FileKind::Symlink {
        disk.bytes.clone()
    } else {
        clean_filter.convert_to_git(path, &disk.bytes, base_index)?
    };
    Ok(hash_object(ObjectKind::Blob, &git_bytes) == expected_oid)
}

fn serialize_switch_index(entries: &BTreeMap<BString, IndexEntryInfo>) -> Result<Vec<u8>> {
    let mut state = gix::index::State::new(gix::hash::Kind::Sha1);
    for (path, info) in entries {
        let gix_oid = gix::ObjectId::from_bytes_or_panic(&info.oid.as_bytes());
        state.dangerously_push_entry(info.stat, gix_oid, info.flags, info.mode, path.as_bstr());
    }
    serialize_index(&mut state)
}

#[allow(clippy::type_complexity)]
fn materialize_written_blobs(
    reader: &mut BoundedReader,
    filter: &mut RepoFilter<'_>,
    live_map: &BTreeMap<BString, EntryState>,
    write_targets: &BTreeMap<BString, (FileKind, GitOid)>,
    policy: CapturePolicy,
) -> Result<(
    BTreeMap<BString, (EntryState, Vec<u8>)>,
    Vec<Vec<u8>>,
    Vec<(BString, Vec<u8>)>,
)> {
    let max_file = usize::try_from(policy.max_file_bytes)
        .unwrap_or(usize::MAX)
        .min(MAX_CAS_BYTES);
    let mut out = BTreeMap::new();
    let mut cas_writes = Vec::new();
    let mut gitignores = Vec::new();
    for (path, (kind, oid)) in write_targets {
        let obj = reader.read(*oid)?;
        if obj.kind != ObjectKind::Blob {
            return Err(Error::CorruptObject(format!(
                "tree entry {} points to {}, expected blob",
                escape_path_bytes(path.as_slice()),
                obj.kind
            )));
        }
        let worktree_bytes = if *kind == FileKind::Symlink {
            if obj.data.len() > MAX_SYMLINK_TARGET_BYTES {
                return Err(Error::UnsupportedRepository(format!(
                    "target symlink {} exceeds maximum target length",
                    escape_path_bytes(path.as_slice())
                )));
            }
            obj.data
        } else {
            filter.convert_to_worktree(path.as_slice(), &obj.data, max_file)?
        };
        if worktree_bytes.len() > max_file {
            return Err(Error::UnsupportedRepository(format!(
                "target file {} exceeds persisted size limit",
                escape_path_bytes(path.as_slice())
            )));
        }
        let state = EntryState {
            kind: *kind,
            content: ContentId::for_bytes(&worktree_bytes),
        };
        if *kind != FileKind::Symlink && is_gitignore_path(path.as_slice()) {
            gitignores.push((path.clone(), worktree_bytes.clone()));
        }
        if live_map.get(path) != Some(&state) {
            cas_writes.push(worktree_bytes.clone());
        }
        out.insert(path.clone(), (state, worktree_bytes));
    }
    Ok((out, cas_writes, gitignores))
}

struct SwitchWorktreeScan {
    existing_dirs: BTreeSet<BString>,
    dirs_with_kept_entries: BTreeSet<BString>,
    newly_exposed_ignored: Vec<(BString, EntryState, Vec<u8>)>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn inspect_switch_worktree(
    worktree: &CheckedRoot,
    policy: CapturePolicy,
    live_map: &BTreeMap<BString, EntryState>,
    remove_paths: &BTreeSet<BString>,
    kept_tracked: &BTreeSet<BString>,
    write_blobs: &BTreeMap<BString, (EntryState, Vec<u8>)>,
    write_dirs: &BTreeSet<BString>,
    write_file_set: &BTreeSet<&[u8]>,
    current_profile: &IgnoreProfile,
    target_profile: &IgnoreProfile,
) -> Result<SwitchWorktreeScan> {
    let live_dirs = ancestor_dir_set(live_map.keys().map(|p| p.as_slice()));
    let mut scan = SwitchWorktreeScan {
        existing_dirs: BTreeSet::new(),
        dirs_with_kept_entries: BTreeSet::new(),
        newly_exposed_ignored: Vec::new(),
    };
    let mut queue = VecDeque::new();
    queue.push_back(BString::default());

    while let Some(dir) = queue.pop_front() {
        for (name, kind) in worktree.list_dir_entries(dir.as_slice())? {
            if dir.is_empty() && name.as_slice() == b".git" {
                continue;
            }
            let rel = join_rel(dir.as_slice(), name.as_slice());
            let under_write_file = has_ancestor_in(rel.as_slice(), write_file_set);
            match kind {
                PathEntryKind::Directory => {
                    if name.as_slice() == b".git" && !dir.is_empty() {
                        check_nested_git_dir(
                            dir.as_slice(),
                            under_write_file,
                            write_dirs.contains(&dir),
                            target_profile,
                            "switch",
                        )?;
                        continue;
                    }
                    scan.existing_dirs.insert(rel.clone());
                    let must_descend = write_blobs.contains_key(&rel)
                        || under_write_file
                        || write_dirs.contains(&rel)
                        || live_dirs.contains(&rel)
                        || !current_profile.is_ignored(rel.as_slice(), true)
                        || !target_profile.is_ignored(rel.as_slice(), true);
                    if must_descend {
                        queue.push_back(rel);
                    } else {
                        mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                    }
                }
                PathEntryKind::Other => {
                    if under_write_file
                        || write_blobs.contains_key(&rel)
                        || write_dirs.contains(&rel)
                        || live_map.contains_key(&rel)
                        || !current_profile.is_ignored(rel.as_slice(), false)
                        || !target_profile.is_ignored(rel.as_slice(), false)
                    {
                        return Err(Error::UnsupportedRepository(format!(
                            "unsupported worktree entry at {} blocks switch",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                }
                PathEntryKind::Regular | PathEntryKind::Executable | PathEntryKind::Symlink => {
                    if live_map.contains_key(&rel) {
                        if remove_paths.contains(&rel) || write_blobs.contains_key(&rel) {
                            continue;
                        }
                        if under_write_file {
                            return Err(Error::UnsupportedRepository(format!(
                                "local entry {} blocks replacement by switch",
                                escape_path_bytes(rel.as_slice())
                            )));
                        }
                        if write_dirs.contains(&rel) {
                            return Err(Error::UnsupportedRepository(format!(
                                "local entry {} blocks target directory in switch",
                                escape_path_bytes(rel.as_slice())
                            )));
                        }
                        if kept_tracked.contains(&rel)
                            || !target_profile.is_ignored(rel.as_slice(), false)
                        {
                            mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                        }
                        continue;
                    }
                    if under_write_file {
                        return Err(Error::UnsupportedRepository(format!(
                            "ignored entry {} blocks directory replacement",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    if write_blobs.contains_key(&rel) {
                        return Err(Error::UnsupportedRepository(format!(
                            "ignored entry {} would be overwritten by switch",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    if write_dirs.contains(&rel) {
                        return Err(Error::UnsupportedRepository(format!(
                            "ignored entry {} blocks target directory",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    if !target_profile.is_ignored(rel.as_slice(), false) {
                        let captured = worktree
                            .read_entry(rel.as_slice(), policy.max_file_bytes)?
                            .ok_or_else(|| {
                                Error::UnsupportedRepository(format!(
                                    "entry {} disappeared during switch planning",
                                    escape_path_bytes(rel.as_slice())
                                ))
                            })?;
                        let disk_state = EntryState {
                            kind: captured.kind,
                            content: ContentId::for_bytes(&captured.bytes),
                        };
                        scan.newly_exposed_ignored
                            .push((rel.clone(), disk_state, captured.bytes));
                    }
                    mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                }
            }
        }
    }

    Ok(scan)
}

#[allow(clippy::too_many_arguments)]
fn build_switch_actions(
    live_map: &BTreeMap<BString, EntryState>,
    remove_paths: &BTreeSet<BString>,
    write_blobs: &BTreeMap<BString, (EntryState, Vec<u8>)>,
    write_dirs: &BTreeSet<BString>,
    write_file_set: &BTreeSet<&[u8]>,
    live_index_id: Option<ContentId>,
    new_index_id: ContentId,
    scan: &SwitchWorktreeScan,
) -> Vec<Action> {
    let mut actions = Vec::new();
    let mut removed_on_disk = BTreeSet::new();
    let mut remove_actions = Vec::new();

    for path in remove_paths {
        if let Some(before) = live_map.get(path) {
            removed_on_disk.insert(path.clone());
            remove_actions.push((
                path.clone(),
                Action::RemoveWorktreeEntry {
                    path: path.clone(),
                    expected_before: *before,
                },
            ));
        }
    }
    remove_actions.sort_by(|left, right| {
        Reverse(component_count(left.0.as_slice()))
            .cmp(&Reverse(component_count(right.0.as_slice())))
            .then_with(|| Reverse(&left.0).cmp(&Reverse(&right.0)))
    });
    for (_, action) in remove_actions {
        actions.push(action);
    }

    push_dir_lifecycle_actions(
        &mut actions,
        &scan.existing_dirs,
        write_dirs,
        write_file_set,
        &scan.dirs_with_kept_entries,
        &removed_on_disk,
    );

    for (path, (target, _)) in write_blobs {
        if live_map.get(path) == Some(target) {
            continue;
        }
        actions.push(Action::WriteWorktreeEntry {
            path: path.clone(),
            expected_before: live_map.get(path).copied(),
            target: *target,
        });
    }

    if live_index_id != Some(new_index_id) {
        actions.push(Action::WriteIndex {
            expected_before: live_index_id,
            target: new_index_id,
        });
    }

    actions
}

fn build_target_files(
    live_map: &BTreeMap<BString, EntryState>,
    remove_paths: &BTreeSet<BString>,
    kept_tracked: &BTreeSet<BString>,
    write_blobs: &BTreeMap<BString, (EntryState, Vec<u8>)>,
    target_profile: &IgnoreProfile,
    newly_exposed_ignored: &[(BString, EntryState, Vec<u8>)],
    cas_writes: &mut Vec<Vec<u8>>,
) -> Vec<FileRecord> {
    let mut target_files = BTreeMap::new();
    for (path, state) in live_map {
        if remove_paths.contains(path) || write_blobs.contains_key(path) {
            continue;
        }
        if kept_tracked.contains(path) || !target_profile.is_ignored(path.as_slice(), false) {
            target_files.insert(
                path.clone(),
                FileRecord {
                    path: path.clone(),
                    kind: state.kind,
                    content: state.content,
                },
            );
        }
    }
    for (path, (state, _)) in write_blobs {
        target_files.insert(
            path.clone(),
            FileRecord {
                path: path.clone(),
                kind: state.kind,
                content: state.content,
            },
        );
    }
    for (path, state, bytes) in newly_exposed_ignored {
        cas_writes.push(bytes.clone());
        target_files.insert(
            path.clone(),
            FileRecord {
                path: path.clone(),
                kind: state.kind,
                content: state.content,
            },
        );
    }
    target_files.into_values().collect()
}
