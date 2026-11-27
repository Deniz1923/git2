//! Read-only verified restore planning and transactional restore execution.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bstr::{BString, ByteSlice};
use gix::ignore::glob::pattern::Case;
use gix::ignore::search::Ignore;

use crate::paths::{CheckedRoot, PathEntryKind, checked_split};
use crate::recovery::{CapturePolicy, ContentId, FileKind, MAX_CAS_BYTES, RecoveryStore, View};
use crate::repository::{LocalRepository, decode_index, validate_index};
use crate::retention;
use crate::store::ObjectStore;
use crate::transaction::{self, Action, EntryState, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

pub(crate) const MAX_SYMLINK_TARGET_BYTES: usize = 64 * 1024;

/// Plan a verified, ignore-aware restore from the current live state to `target`.
///
/// This performs zero recovery-lock acquisitions and zero writes to `.git/` or `.git/git2/`.
///
/// # Errors
///
/// Returns an error if a pending transaction exists, `target` is missing or corrupt, any
/// required target object fails verification, ignore-rule changes leave an uncaptured entry
/// ambiguous, or an ignored entry blocks replacement or directory creation.
pub fn plan(repo: &LocalRepository, target: ContentId) -> Result<MutationPlan> {
    plan_with_command(
        repo,
        target,
        BString::from(format!("restore --view {target}")),
    )
}

pub(crate) fn plan_with_command(
    repo: &LocalRepository,
    target: ContentId,
    command: BString,
) -> Result<MutationPlan> {
    let store = RecoveryStore::open(repo.git_dir())?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }
    let expected_head = store.head()?;
    let policy = store.policy()?;
    let target_view = store.read_view(target)?;
    let target_index = verify_target_view(&store, &target_view, policy)?;

    let (live_view_id, live_view) = repo.capture_live_view_readonly(policy)?;
    let live_index = decode_view_index_readonly(repo.git_dir(), &live_view)?;

    let excludes_file = repo.global_excludes_file()?;
    let case = if repo.ignore_case() {
        Case::Fold
    } else {
        Case::Sensitive
    };
    let target_profile = IgnoreProfile::for_target(
        repo.git_dir(),
        repo.workdir(),
        excludes_file.clone(),
        case,
        target_index.as_ref(),
        &target_view,
        &store,
        policy,
    )?;

    let live_map = file_map(&live_view);
    let target_map = file_map(&target_view);
    verify_target_files_eligible(&target_map, &target_profile)?;

    let worktree = CheckedRoot::open(repo.workdir())?;
    let current_profile = IgnoreProfile::for_live(
        repo.git_dir(),
        repo.workdir(),
        &worktree,
        excludes_file,
        case,
        live_index.as_ref(),
        target_index.as_ref(),
        policy,
    )?;

    let scan = inspect_worktree(
        &worktree,
        policy,
        &live_map,
        &target_map,
        &current_profile,
        &target_profile,
    )?;
    let plan_ctx = WorktreePlanContext {
        live_map: &live_map,
        target_map: &target_map,
        target_profile: &target_profile,
        scan,
    };
    let mut actions = collect_rehydrate_actions(repo.git_dir(), &target_view)?;
    append_worktree_actions(&mut actions, &plan_ctx)?;
    append_metadata_actions(&mut actions, &live_view, &target_view);

    let changes = MutationPlan::changes_for_actions(&actions);
    Ok(MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target,
        command,
        actions,
        changes,
    })
}

/// Apply a verified restore to `target`, returning the durable operation ID.
///
/// # Errors
///
/// Returns an error if planning fails, concurrent state diverges, a foreign Git lock exists,
/// or a transaction/storage failure prevents durable completion.
pub fn apply(repo: &LocalRepository, target: ContentId) -> Result<ContentId> {
    apply_with_hook(repo, target, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_with_hook(
    repo: &LocalRepository,
    target: ContentId,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<ContentId> {
    apply_with_command_and_hook(
        repo,
        target,
        BString::from(format!("restore --view {target}")),
        hook,
    )
}

pub(crate) fn apply_with_command_and_hook(
    repo: &LocalRepository,
    target: ContentId,
    command: BString,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<ContentId> {
    repo.import_external_changes()?;
    let mutation_plan = plan_with_command(repo, target, command)?;
    ensure_no_foreign_locks(repo.git_dir(), &mutation_plan.actions)?;
    transaction::run_with_hook(repo, &mutation_plan, hook)
}

pub(crate) fn ensure_no_foreign_locks(git_dir: &Path, actions: &[Action]) -> Result<()> {
    let git_meta = CheckedRoot::open(git_dir)?;
    for action in actions {
        let lock_path: Option<BString> = match action {
            Action::WriteIndex { .. } | Action::RemoveIndex { .. } => {
                Some(BString::from(b"index.lock".to_vec()))
            }
            Action::WriteRef { name, .. } | Action::RemoveRef { name, .. } => {
                let mut lock = name.clone();
                lock.extend_from_slice(b".lock");
                Some(lock)
            }
            Action::WritePackedRefs { .. } => Some(BString::from(b"packed-refs.lock".to_vec())),
            Action::WriteHead { .. } => Some(BString::from(b"HEAD.lock".to_vec())),
            _ => None,
        };
        if let Some(lock_rel) = lock_path {
            match git_meta.entry_kind(lock_rel.as_slice()) {
                Ok(Some(_)) => {
                    return Err(Error::UnsupportedRepository(format!(
                        "unknown Git lock {} blocks restore",
                        escape_path_bytes(lock_rel.as_slice())
                    )));
                }
                Ok(None) => {}
                Err(Error::Io(error))
                    if error.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error()) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn verify_target_view(
    store: &RecoveryStore,
    view: &View,
    policy: CapturePolicy,
) -> Result<Option<gix::index::State>> {
    retention::verify(store, view)?;
    let index = view
        .index
        .map(|id| store.read_content_bounded(id, MAX_CAS_BYTES))
        .transpose()?;
    let decoded = index.as_deref().map(decode_index).transpose()?;
    if let Some(index_state) = &decoded {
        validate_index(index_state)?;
    }

    let max_bytes = usize::try_from(policy.max_file_bytes)
        .unwrap_or(usize::MAX)
        .min(MAX_CAS_BYTES);
    let mut seen_paths = BTreeSet::new();
    for file in &view.files {
        checked_split(file.path.as_slice())?;
        if file.path.starts_with(b".git/") || file.path.as_slice() == b".git" {
            return Err(Error::CorruptRecovery(format!(
                "target view contains forbidden Git metadata path {}",
                escape_path_bytes(file.path.as_slice())
            )));
        }
        if !seen_paths.insert(file.path.as_slice()) {
            return Err(Error::CorruptRecovery(format!(
                "target view contains duplicate path {}",
                escape_path_bytes(file.path.as_slice())
            )));
        }
        let bytes = store.read_content_bounded(file.content, max_bytes)?;
        if file.kind == FileKind::Symlink && bytes.len() > MAX_SYMLINK_TARGET_BYTES {
            return Err(Error::UnsupportedRepository(format!(
                "symlink {} in target view exceeds maximum target length",
                escape_path_bytes(file.path.as_slice())
            )));
        }
    }
    for path in &seen_paths {
        if has_ancestor_in(path, &seen_paths) {
            return Err(Error::CorruptRecovery(format!(
                "target view has conflicting file and directory at {}",
                escape_path_bytes(path)
            )));
        }
    }
    Ok(decoded)
}

pub(crate) fn decode_view_index_readonly(
    git_dir: &Path,
    view: &View,
) -> Result<Option<gix::index::State>> {
    let Some(expected_id) = view.index else {
        return Ok(None);
    };
    let meta = CheckedRoot::open(git_dir)?;
    let Some(bytes) = meta.read_file_raw(b"index", MAX_CAS_BYTES as u64)? else {
        return Ok(None);
    };
    if ContentId::for_bytes(&bytes) != expected_id {
        return Err(Error::UnsupportedRepository(
            "Git index changed concurrently during restore planning".to_owned(),
        ));
    }
    Ok(Some(decode_index(&bytes)?))
}

fn verify_target_files_eligible(
    target_map: &BTreeMap<BString, EntryState>,
    target_profile: &IgnoreProfile,
) -> Result<()> {
    for path in target_map.keys() {
        if target_profile.is_ignored(path.as_slice(), false) {
            return Err(Error::UnsupportedRepository(format!(
                "cannot verify target path {} because effective ignore rules ignore it without tracking it",
                escape_path_bytes(path.as_slice())
            )));
        }
    }
    Ok(())
}

pub(crate) struct IgnoreProfile {
    search: gix::ignore::Search,
    tracked: BTreeSet<BString>,
    case: Case,
}

impl IgnoreProfile {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_gitignore_blobs(
        git_dir: &Path,
        workdir: &Path,
        excludes_file: Option<std::path::PathBuf>,
        case: Case,
        index: Option<&gix::index::State>,
        mut gitignores: Vec<(BString, Vec<u8>)>,
    ) -> Result<Self> {
        let mut buf = Vec::new();
        let mut search =
            gix::ignore::Search::from_git_dir(git_dir, excludes_file, &mut buf, Ignore::default())?;
        gitignores.sort_by(|left, right| {
            component_count(left.0.as_slice())
                .cmp(&component_count(right.0.as_slice()))
                .then_with(|| left.0.cmp(&right.0))
        });
        for (path, bytes) in gitignores {
            let source = workdir.join(OsStr::from_bytes(path.as_slice()));
            search.add_patterns_buffer(&bytes, source, Some(workdir), Ignore::default());
        }
        Ok(Self {
            search,
            tracked: tracked_set(index),
            case,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_target(
        git_dir: &Path,
        workdir: &Path,
        excludes_file: Option<std::path::PathBuf>,
        case: Case,
        index: Option<&gix::index::State>,
        view: &View,
        store: &RecoveryStore,
        policy: CapturePolicy,
    ) -> Result<Self> {
        let max_bytes = usize::try_from(policy.max_file_bytes)
            .unwrap_or(usize::MAX)
            .min(MAX_CAS_BYTES);
        let mut gitignores = Vec::new();
        for file in &view.files {
            if file.kind != FileKind::Symlink && is_gitignore_path(file.path.as_slice()) {
                let bytes = store.read_content_bounded(file.content, max_bytes)?;
                gitignores.push((file.path.clone(), bytes));
            }
        }
        Self::from_gitignore_blobs(git_dir, workdir, excludes_file, case, index, gitignores)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn for_live(
        git_dir: &Path,
        workdir: &Path,
        worktree: &CheckedRoot,
        excludes_file: Option<std::path::PathBuf>,
        case: Case,
        live_index: Option<&gix::index::State>,
        target_index: Option<&gix::index::State>,
        policy: CapturePolicy,
    ) -> Result<Self> {
        let mut buf = Vec::new();
        let search =
            gix::ignore::Search::from_git_dir(git_dir, excludes_file, &mut buf, Ignore::default())?;
        let mut profile = Self {
            search,
            tracked: tracked_set(live_index),
            case,
        };
        let mut descend_dirs = tracked_ancestor_dirs(live_index);
        descend_dirs.extend(tracked_ancestor_dirs(target_index));

        let mut queue = VecDeque::new();
        queue.push_back(BString::default());
        while let Some(dir) = queue.pop_front() {
            let entries = worktree.list_dir_entries(dir.as_slice())?;
            if let Some((_, kind)) = entries
                .iter()
                .find(|(name, _)| name.as_slice() == b".gitignore")
                && matches!(kind, PathEntryKind::Regular | PathEntryKind::Executable)
            {
                let rel = join_rel(dir.as_slice(), b".gitignore");
                if let Some(captured) =
                    worktree.read_entry(rel.as_slice(), policy.max_file_bytes)?
                {
                    let source = workdir.join(OsStr::from_bytes(rel.as_slice()));
                    profile.search.add_patterns_buffer(
                        &captured.bytes,
                        source,
                        Some(workdir),
                        Ignore::default(),
                    );
                }
            }
            for (name, kind) in entries {
                if dir.is_empty() && name.as_slice() == b".git" {
                    continue;
                }
                if kind == PathEntryKind::Directory {
                    let sub = join_rel(dir.as_slice(), name.as_slice());
                    if !profile.is_ignored(sub.as_slice(), true) || descend_dirs.contains(&sub) {
                        queue.push_back(sub);
                    }
                }
            }
        }
        Ok(profile)
    }

    pub(crate) fn is_ignored(&self, path: &[u8], is_dir: bool) -> bool {
        if !is_dir && self.tracked.contains(path.as_bstr()) {
            return false;
        }
        let mut prefix = Vec::with_capacity(path.len());
        let mut iter = path.split(|&b| b == b'/').peekable();
        while let Some(part) = iter.next() {
            if !prefix.is_empty() {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(part);
            if iter.peek().is_some()
                && matches!(
                    self.search.pattern_matching_relative_path(
                        prefix.as_bstr(),
                        Some(true),
                        self.case
                    ),
                    Some(matched) if !matched.pattern.is_negative()
                )
            {
                return true;
            }
        }
        matches!(
            self.search
                .pattern_matching_relative_path(path.as_bstr(), Some(is_dir), self.case),
            Some(matched) if !matched.pattern.is_negative()
        )
    }
}

struct WorktreeScan {
    existing_dirs: BTreeSet<BString>,
    dirs_with_kept_entries: BTreeSet<BString>,
    reclassified_keep: BTreeSet<BString>,
}

fn inspect_worktree(
    worktree: &CheckedRoot,
    policy: CapturePolicy,
    live_map: &BTreeMap<BString, EntryState>,
    target_map: &BTreeMap<BString, EntryState>,
    current_profile: &IgnoreProfile,
    target_profile: &IgnoreProfile,
) -> Result<WorktreeScan> {
    let target_dirs = ancestor_dir_set(target_map.keys().map(|p| p.as_slice()));
    let live_dirs = ancestor_dir_set(live_map.keys().map(|p| p.as_slice()));
    let target_file_set: BTreeSet<&[u8]> = target_map.keys().map(|p| p.as_slice()).collect();

    let mut scan = WorktreeScan {
        existing_dirs: BTreeSet::new(),
        dirs_with_kept_entries: BTreeSet::new(),
        reclassified_keep: BTreeSet::new(),
    };
    let mut queue = VecDeque::new();
    queue.push_back(BString::default());

    while let Some(dir) = queue.pop_front() {
        let entries = worktree.list_dir_entries(dir.as_slice())?;
        for (name, kind) in entries {
            if dir.is_empty() && name.as_slice() == b".git" {
                continue;
            }
            let rel = join_rel(dir.as_slice(), name.as_slice());
            let under_target_file = has_ancestor_in(rel.as_slice(), &target_file_set);
            match kind {
                PathEntryKind::Directory => {
                    if name.as_slice() == b".git" && !dir.is_empty() {
                        check_nested_git_dir(
                            dir.as_slice(),
                            under_target_file,
                            target_dirs.contains(&dir),
                            target_profile,
                            "restore",
                        )?;
                        continue;
                    }
                    scan.existing_dirs.insert(rel.clone());
                    let must_descend = target_map.contains_key(&rel)
                        || under_target_file
                        || target_dirs.contains(&rel)
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
                    if under_target_file
                        || target_map.contains_key(&rel)
                        || target_dirs.contains(&rel)
                        || live_map.contains_key(&rel)
                        || !current_profile.is_ignored(rel.as_slice(), false)
                        || !target_profile.is_ignored(rel.as_slice(), false)
                    {
                        return Err(Error::UnsupportedRepository(format!(
                            "unsupported worktree entry at {} blocks restore",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                }
                PathEntryKind::Regular | PathEntryKind::Executable | PathEntryKind::Symlink => {
                    if !live_map.contains_key(&rel) {
                        inspect_non_live_entry(
                            worktree,
                            policy,
                            &rel,
                            under_target_file,
                            target_map,
                            &target_dirs,
                            target_profile,
                            &mut scan,
                        )?;
                    }
                }
            }
        }
    }
    Ok(scan)
}

#[allow(clippy::too_many_arguments)]
fn inspect_non_live_entry(
    worktree: &CheckedRoot,
    policy: CapturePolicy,
    rel: &BString,
    under_target_file: bool,
    target_map: &BTreeMap<BString, EntryState>,
    target_dirs: &BTreeSet<BString>,
    target_profile: &IgnoreProfile,
    scan: &mut WorktreeScan,
) -> Result<()> {
    if under_target_file {
        return Err(Error::UnsupportedRepository(format!(
            "ignored entry {} blocks directory replacement without writes",
            escape_path_bytes(rel.as_slice())
        )));
    }
    if let Some(target_entry) = target_map.get(rel) {
        let captured = worktree
            .read_entry(rel.as_slice(), policy.max_file_bytes)?
            .ok_or_else(|| {
                Error::UnsupportedRepository(format!(
                    "entry {} disappeared during restore planning",
                    escape_path_bytes(rel.as_slice())
                ))
            })?;
        let disk_state = EntryState {
            kind: captured.kind,
            content: ContentId::for_bytes(&captured.bytes),
        };
        if disk_state == *target_entry {
            scan.reclassified_keep.insert(rel.clone());
            return Ok(());
        }
        return Err(Error::UnsupportedRepository(format!(
            "ignored entry {} conflicts with target path",
            escape_path_bytes(rel.as_slice())
        )));
    }
    if target_dirs.contains(rel) {
        return Err(Error::UnsupportedRepository(format!(
            "ignored entry {} blocks target directory",
            escape_path_bytes(rel.as_slice())
        )));
    }
    if !target_profile.is_ignored(rel.as_slice(), false) {
        return Err(Error::UnsupportedRepository(format!(
            "currently ignored entry {} would become non-ignored untracked in target state",
            escape_path_bytes(rel.as_slice())
        )));
    }
    mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
    Ok(())
}

struct WorktreePlanContext<'a> {
    live_map: &'a BTreeMap<BString, EntryState>,
    target_map: &'a BTreeMap<BString, EntryState>,
    target_profile: &'a IgnoreProfile,
    scan: WorktreeScan,
}

fn append_worktree_actions(actions: &mut Vec<Action>, ctx: &WorktreePlanContext<'_>) -> Result<()> {
    let target_dirs = ancestor_dir_set(ctx.target_map.keys().map(|p| p.as_slice()));
    let target_file_set: BTreeSet<&[u8]> = ctx.target_map.keys().map(|p| p.as_slice()).collect();
    let mut dirs_with_kept = ctx.scan.dirs_with_kept_entries.clone();
    let mut removed_paths = BTreeSet::new();
    let mut remove_actions = Vec::new();
    let mut kept_live_files = BTreeSet::new();

    for (path, before) in ctx.live_map {
        if ctx.target_map.contains_key(path) {
            continue;
        }
        let keep_reclassified = ctx.target_profile.is_ignored(path.as_slice(), false)
            && !target_dirs.contains(path)
            && !has_ancestor_in(path.as_slice(), &target_file_set);
        if keep_reclassified {
            mark_ancestors(path.as_slice(), &mut dirs_with_kept);
            kept_live_files.insert(path.clone());
            continue;
        }
        removed_paths.insert(path.clone());
        remove_actions.push((
            path.clone(),
            Action::RemoveWorktreeEntry {
                path: path.clone(),
                expected_before: *before,
            },
        ));
    }
    remove_actions.sort_by(|left, right| {
        Reverse(component_count(left.0.as_slice()))
            .cmp(&Reverse(component_count(right.0.as_slice())))
            .then_with(|| Reverse(&left.0).cmp(&Reverse(&right.0)))
    });
    for (_, action) in remove_actions {
        actions.push(action);
    }

    let kept_set: BTreeSet<&[u8]> = kept_live_files.iter().map(|p| p.as_slice()).collect();
    for path in ctx.target_map.keys() {
        if has_ancestor_in(path.as_slice(), &kept_set) {
            return Err(Error::UnsupportedRepository(format!(
                "reclassified entry blocks target directory for {}",
                escape_path_bytes(path.as_slice())
            )));
        }
    }

    push_dir_lifecycle_actions(
        actions,
        &ctx.scan.existing_dirs,
        &target_dirs,
        &target_file_set,
        &dirs_with_kept,
        &removed_paths,
    );

    for (path, target) in ctx.target_map {
        if ctx.scan.reclassified_keep.contains(path) || ctx.live_map.get(path) == Some(target) {
            continue;
        }
        actions.push(Action::WriteWorktreeEntry {
            path: path.clone(),
            expected_before: ctx.live_map.get(path).copied(),
            target: *target,
        });
    }
    Ok(())
}

fn collect_rehydrate_actions(git_dir: &Path, target_view: &View) -> Result<Vec<Action>> {
    let objects = ObjectStore::open(git_dir)?;
    let mut reader = objects.bounded_reader(retention::MAX_OBJECT_BYTES)?;
    let mut actions = Vec::new();
    for retained in &target_view.retained_objects {
        match reader.read_physical(retained.oid) {
            Ok(_) => {}
            Err(Error::ObjectNotFound(_)) => actions.push(Action::RehydrateObject {
                oid: retained.oid,
                content: retained.content,
            }),
            Err(error) => return Err(error),
        }
    }
    Ok(actions)
}

fn append_metadata_actions(actions: &mut Vec<Action>, live_view: &View, target_view: &View) {
    match (live_view.index, target_view.index) {
        (before, Some(after)) if before != Some(after) => actions.push(Action::WriteIndex {
            expected_before: before,
            target: after,
        }),
        (Some(before), None) => actions.push(Action::RemoveIndex {
            expected_before: before,
        }),
        _ => {}
    }

    let (live_packed, live_loose) = split_refs(&live_view.refs);
    let (target_packed, target_loose) = split_refs(&target_view.refs);

    let mut remove_refs: Vec<(BString, BString)> = live_loose
        .iter()
        .filter(|(name, _)| !target_loose.contains_key(*name))
        .map(|(name, val)| (name.clone(), val.clone()))
        .collect();
    remove_refs.sort_by(|left, right| {
        Reverse(component_count(left.0.as_slice()))
            .cmp(&Reverse(component_count(right.0.as_slice())))
            .then_with(|| Reverse(&left.0).cmp(&Reverse(&right.0)))
    });
    for (name, expected_before) in remove_refs {
        actions.push(Action::RemoveRef {
            name,
            expected_before,
        });
    }

    let mut write_refs: Vec<(BString, Option<BString>, BString)> = target_loose
        .iter()
        .filter_map(|(name, target_val)| {
            let before = live_loose.get(name).cloned();
            if before.as_ref() == Some(target_val) {
                None
            } else {
                Some((name.clone(), before, target_val.clone()))
            }
        })
        .collect();
    write_refs.sort_by(|left, right| {
        component_count(left.0.as_slice())
            .cmp(&component_count(right.0.as_slice()))
            .then_with(|| left.0.cmp(&right.0))
    });
    for (name, expected_before, target) in write_refs {
        actions.push(Action::WriteRef {
            name,
            expected_before,
            target,
        });
    }

    if live_packed != target_packed {
        actions.push(Action::WritePackedRefs {
            expected_before: live_packed,
            target: target_packed,
        });
    }

    if live_view.head != target_view.head {
        actions.push(Action::WriteHead {
            expected_before: live_view.head.clone(),
            target: target_view.head.clone(),
        });
    }
}

pub(crate) fn file_map(view: &View) -> BTreeMap<BString, EntryState> {
    view.files
        .iter()
        .map(|file| {
            (
                file.path.clone(),
                EntryState {
                    kind: file.kind,
                    content: file.content,
                },
            )
        })
        .collect()
}

pub(crate) fn split_refs(
    refs: &[(BString, BString)],
) -> (Option<BString>, BTreeMap<BString, BString>) {
    let mut packed = None;
    let mut loose = BTreeMap::new();
    for (name, val) in refs {
        if name.as_slice() == b"packed-refs" {
            packed = Some(val.clone());
        } else {
            loose.insert(name.clone(), val.clone());
        }
    }
    (packed, loose)
}

pub(crate) fn tracked_set(index: Option<&gix::index::State>) -> BTreeSet<BString> {
    let Some(index) = index else {
        return BTreeSet::new();
    };
    index
        .entries()
        .iter()
        .map(|entry| BString::from(entry.path(index).to_vec()))
        .collect()
}

fn tracked_ancestor_dirs(index: Option<&gix::index::State>) -> BTreeSet<BString> {
    let Some(index) = index else {
        return BTreeSet::new();
    };
    ancestor_dir_set(
        index
            .entries()
            .iter()
            .map(|entry| entry.path(index).as_ref()),
    )
}

pub(crate) fn ancestor_dir_set<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> BTreeSet<BString> {
    let mut dirs = BTreeSet::new();
    for path in paths {
        mark_ancestors(path, &mut dirs);
    }
    dirs
}

pub(crate) fn mark_ancestors(path: &[u8], out: &mut BTreeSet<BString>) {
    let mut cur = path;
    while let Some(slash) = cur.rfind_byte(b'/') {
        cur = &cur[..slash];
        if !out.insert(BString::from(cur)) {
            break;
        }
    }
}

pub(crate) fn has_ancestor_in(path: &[u8], set: &BTreeSet<&[u8]>) -> bool {
    let mut cur = path;
    while let Some(slash) = cur.rfind_byte(b'/') {
        cur = &cur[..slash];
        if set.contains(cur) {
            return true;
        }
    }
    false
}

pub(crate) fn is_gitignore_path(path: &[u8]) -> bool {
    path == b".gitignore" || path.ends_with(b"/.gitignore")
}

pub(crate) fn join_rel(dir: &[u8], name: &[u8]) -> BString {
    if dir.is_empty() {
        BString::from(name)
    } else {
        let mut out = Vec::with_capacity(dir.len() + 1 + name.len());
        out.extend_from_slice(dir);
        out.push(b'/');
        out.extend_from_slice(name);
        BString::from(out)
    }
}

pub(crate) fn component_count(path: &[u8]) -> usize {
    if path.is_empty() {
        0
    } else {
        path.split(|&b| b == b'/').count()
    }
}

pub(crate) fn check_nested_git_dir(
    dir: &[u8],
    under_target_file: bool,
    target_has_dir: bool,
    target_profile: &IgnoreProfile,
    context: &str,
) -> Result<()> {
    if under_target_file || target_has_dir || !target_profile.is_ignored(dir, true) {
        return Err(Error::UnsupportedRepository(format!(
            "nested repository at {} blocks {context}",
            escape_path_bytes(dir)
        )));
    }
    Ok(())
}

pub(crate) fn push_dir_lifecycle_actions(
    actions: &mut Vec<Action>,
    existing_dirs: &BTreeSet<BString>,
    target_dirs: &BTreeSet<BString>,
    target_file_set: &BTreeSet<&[u8]>,
    dirs_with_kept: &BTreeSet<BString>,
    removed_paths: &BTreeSet<BString>,
) {
    let mut dirs_to_remove: Vec<BString> = existing_dirs
        .iter()
        .filter(|dir| {
            let s = dir.as_slice();
            let replaced_by_file =
                target_file_set.contains(s) || has_ancestor_in(s, target_file_set);
            let orphaned = !target_dirs.contains(*dir) && !dirs_with_kept.contains(*dir);
            replaced_by_file || orphaned
        })
        .cloned()
        .collect();
    dirs_to_remove.sort_by(|left, right| {
        Reverse(component_count(left.as_slice()))
            .cmp(&Reverse(component_count(right.as_slice())))
            .then_with(|| Reverse(left).cmp(&Reverse(right)))
    });
    for dir in dirs_to_remove {
        actions.push(Action::RemoveEmptyDir { path: dir });
    }

    let mut dirs_to_ensure: Vec<BString> = target_dirs
        .iter()
        .filter(|dir| !existing_dirs.contains(*dir) || removed_paths.contains(*dir))
        .cloned()
        .collect();
    dirs_to_ensure.sort_by(|left, right| {
        component_count(left.as_slice())
            .cmp(&component_count(right.as_slice()))
            .then_with(|| left.cmp(right))
    });
    for dir in dirs_to_ensure {
        actions.push(Action::EnsureDir { path: dir });
    }
}
