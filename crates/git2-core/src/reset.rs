//! Native `git2 reset [--soft | --mixed | --hard]` over supported commits and branches.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bstr::{BString, ByteSlice};
use gix::ignore::glob::pattern::Case;
use sha1::{Digest, Sha1};

use crate::object::{Commit, GitOid, ObjectKind, Tree, TreeMode};
use crate::paths::{CheckedRoot, PathEntryKind, checked_split};
use crate::recovery::{
    CapturePolicy, ContentId, FileKind, FileRecord, MAX_CAS_BYTES, RecoveryStore, View,
    check_view_size,
};
use crate::repository::{LocalRepository, decode_index, known_cas_for_view, validate_index};
use crate::restore::{
    IgnoreProfile, MAX_SYMLINK_TARGET_BYTES, ancestor_dir_set, check_nested_git_dir,
    component_count, decode_view_index_readonly, ensure_no_foreign_locks, file_map,
    has_ancestor_in, is_gitignore_path, join_rel, mark_ancestors, push_dir_lifecycle_actions,
    split_refs, tracked_set,
};
use crate::retention::{self, MAX_OBJECT_BYTES};
use crate::stage::RepoFilter;
use crate::store::{BoundedReader, ObjectStore};
use crate::transaction::{self, Action, EntryState, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

/// Maximum first-parent hops permitted in a `~N` revision expression.
pub const MAX_FIRST_PARENT_STEPS: usize = 10_000;

/// Mode controlling which repository surfaces `git2 reset` updates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetMode {
    /// Move only HEAD (or its target branch ref).
    Soft,
    /// Move HEAD (or its branch) and rebuild the index from the target tree.
    Mixed,
    /// Move HEAD (or its branch), rebuild the index, and reset tracked worktree paths.
    Hard,
}

impl ResetMode {
    /// Return the CLI spelling of this reset mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Mixed => "mixed",
            Self::Hard => "hard",
        }
    }
}

/// Summary returned after a durable `git2 reset`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetOutcome {
    pub operation_id: ContentId,
    pub target_commit: GitOid,
    pub after_view: ContentId,
}

/// Resolve a commit revision restricted to `HEAD`, full 40-hex commit IDs,
/// local branch names (`refs/heads/<name>`), and optional `~N` suffixes up to
/// [`MAX_FIRST_PARENT_STEPS`].
///
/// # Errors
///
/// Returns an error if the revision uses unsupported syntax, names an unborn or unknown
/// reference, does not resolve to a commit object, or exceeds `~N` parent history/bounds.
pub fn resolve_commit(repo: &LocalRepository, rev: &str) -> Result<GitOid> {
    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    resolve_commit_with_reader(repo, &mut reader, rev)
}

fn resolve_commit_with_reader(
    repo: &LocalRepository,
    reader: &mut BoundedReader,
    rev: &str,
) -> Result<GitOid> {
    if rev.is_empty() || rev.contains('^') || rev.contains("@{") || rev.contains("..") {
        return Err(Error::UnsupportedRepository(format!(
            "unsupported revision syntax {rev:?}; use commit ID, HEAD, branch name, and optional ~N"
        )));
    }
    let (base, steps) = parse_tilde_suffix(rev)?;
    let meta = CheckedRoot::open(repo.git_dir())?;
    let base_oid = resolve_base_oid(&meta, reader, base)?;
    walk_first_parent(reader, base_oid, steps)
}

/// Apply `git2 reset` cleanly without failure injection.
///
/// # Errors
///
/// Returns an error if a pending transaction exists, the revision cannot be resolved,
/// pre-checks refuse an obstructed or unsupported transition, a foreign Git lock exists,
/// or durable execution fails.
pub fn apply(repo: &LocalRepository, mode: ResetMode, rev: &str) -> Result<ResetOutcome> {
    apply_with_hook(repo, mode, rev, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_with_hook(
    repo: &LocalRepository,
    mode: ResetMode,
    rev: &str,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<ResetOutcome> {
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

    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let target_commit = resolve_commit_with_reader(repo, &mut reader, rev)?;
    let commit_obj = reader.read(target_commit)?;
    if commit_obj.kind != ObjectKind::Commit {
        return Err(Error::UnsupportedRepository(format!(
            "object {target_commit} is a {}, expected commit",
            commit_obj.kind
        )));
    }
    let commit = Commit::parse(&commit_obj.data)?;

    let head_plan = plan_head_update(&live_view, target_commit)?;
    let prepared = prepare_reset_state(
        repo,
        &store,
        &mut reader,
        mode,
        commit.tree,
        &live_view,
        live_index.as_ref(),
        policy,
    )?;
    let mut actions = prepared.worktree_and_index_actions;
    actions.extend(head_plan.actions);

    ensure_no_foreign_locks(repo.git_dir(), &actions)?;

    let known_cas = known_cas_for_view(&live_view);
    let target_view_id = persist_target_view(
        &store,
        &objects,
        &known_cas,
        head_plan.head,
        head_plan.refs,
        prepared.target_index_bytes.as_deref(),
        prepared.target_files,
        prepared.cas_writes,
    )?;

    let changes = MutationPlan::changes_for_actions(&actions);
    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from(format!("reset --{} {rev}", mode.as_str())),
        actions,
        changes,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(ResetOutcome {
        operation_id,
        target_commit,
        after_view: target_view_id,
    })
}

fn parse_tilde_suffix(rev: &str) -> Result<(&str, usize)> {
    let Some(tilde) = rev.find('~') else {
        return Ok((rev, 0));
    };
    let base = &rev[..tilde];
    let suffix = &rev[tilde + 1..];
    if base.is_empty() || suffix.contains('~') {
        return Err(Error::UnsupportedRepository(format!(
            "unsupported revision syntax {rev:?}"
        )));
    }
    let steps = if suffix.is_empty() {
        1
    } else {
        if !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::UnsupportedRepository(format!(
                "invalid ~N count in revision {rev:?}"
            )));
        }
        suffix.parse::<usize>().map_err(|_| {
            Error::UnsupportedRepository(format!("revision count out of range in {rev:?}"))
        })?
    };
    if steps > MAX_FIRST_PARENT_STEPS {
        return Err(Error::UnsupportedRepository(format!(
            "revision {rev:?} exceeds maximum {MAX_FIRST_PARENT_STEPS} first-parent steps"
        )));
    }
    Ok((base, steps))
}

fn resolve_base_oid(meta: &CheckedRoot, reader: &mut BoundedReader, base: &str) -> Result<GitOid> {
    if base == "HEAD" {
        let raw_head = meta
            .read_file_raw(b"HEAD", MAX_CAS_BYTES as u64)?
            .ok_or_else(|| Error::UnsupportedRepository("Git HEAD is missing".to_owned()))?;
        let trimmed = trim_newline(&raw_head);
        if let Some(target_ref) = trimmed.strip_prefix(b"ref: ") {
            let (leaf_ref, oid) = resolve_ref_chain(meta, target_ref)?;
            return oid.ok_or_else(|| {
                Error::UnsupportedRepository(format!(
                    "cannot reset unborn branch {}",
                    escape_path_bytes(leaf_ref.as_slice())
                ))
            });
        }
        return GitOid::from_hex(trimmed);
    }

    if base.len() == 40
        && let Ok(oid) = GitOid::from_hex(base.as_bytes())
    {
        if reader.read(oid)?.kind != ObjectKind::Commit {
            return Err(Error::UnsupportedRepository(format!(
                "object {oid} is not a commit"
            )));
        }
        return Ok(oid);
    }

    let branch_ref = format!("refs/heads/{base}");
    gix::validate::reference::name(branch_ref.as_bytes().as_bstr()).map_err(|_| {
        Error::UnsupportedRepository(format!("unsupported revision or branch name {base:?}"))
    })?;
    let (_, oid) = resolve_ref_chain(meta, branch_ref.as_bytes())?;
    oid.ok_or_else(|| {
        Error::UnsupportedRepository(format!("unknown revision or local branch {base:?}"))
    })
}

pub(crate) fn resolve_ref_chain(
    meta: &CheckedRoot,
    start_ref: &[u8],
) -> Result<(BString, Option<GitOid>)> {
    let mut current = BString::from(start_ref);
    for _ in 0..32 {
        checked_split(current.as_slice())?;
        if !current.starts_with(b"refs/") {
            return Err(Error::UnsupportedRepository(format!(
                "unsupported symbolic reference {}",
                escape_path_bytes(current.as_slice())
            )));
        }
        gix::validate::reference::name(current.as_bstr()).map_err(|_| {
            Error::UnsupportedRepository(format!(
                "invalid reference name {}",
                escape_path_bytes(current.as_slice())
            ))
        })?;
        if let Some(loose) = meta.read_file_raw(current.as_slice(), MAX_CAS_BYTES as u64)? {
            let trimmed = trim_newline(&loose);
            if let Some(next_ref) = trimmed.strip_prefix(b"ref: ") {
                current = BString::from(next_ref);
                continue;
            }
            return Ok((current, Some(GitOid::from_hex(trimmed)?)));
        }
        return Ok((
            current.clone(),
            resolve_packed_ref_oid(meta, current.as_slice())?,
        ));
    }
    Err(Error::UnsupportedRepository(
        "symbolic reference exceeds maximum indirection depth".to_owned(),
    ))
}

fn resolve_packed_ref_oid(meta: &CheckedRoot, ref_name: &[u8]) -> Result<Option<GitOid>> {
    let Some(packed) = meta.read_file_raw(b"packed-refs", MAX_CAS_BYTES as u64)? else {
        return Ok(None);
    };
    for entry in gix::refs::packed::Iter::new(&packed, gix::hash::Kind::Sha1)
        .map_err(|error| Error::UnsupportedRepository(format!("invalid packed-refs: {error}")))?
    {
        let reference = entry.map_err(|error| {
            Error::UnsupportedRepository(format!("corrupt packed-refs entry: {error}"))
        })?;
        if reference.name.as_bstr().as_bytes() == ref_name {
            let oid = GitOid::from_hex(reference.target.as_bytes())?;
            return Ok(Some(oid));
        }
    }
    Ok(None)
}

fn walk_first_parent(
    reader: &mut BoundedReader,
    mut current: GitOid,
    steps: usize,
) -> Result<GitOid> {
    for step in 0..=steps {
        let object = reader.read(current)?;
        if object.kind != ObjectKind::Commit {
            return Err(Error::UnsupportedRepository(format!(
                "object {current} is a {}, expected commit",
                object.kind
            )));
        }
        if step == steps {
            break;
        }
        let commit = Commit::parse(&object.data)?;
        current = *commit.parents.first().ok_or_else(|| {
            Error::UnsupportedRepository(format!(
                "commit {current} has no first parent at step {step}"
            ))
        })?;
    }
    Ok(current)
}

pub(crate) struct HeadPlan {
    pub(crate) head: BString,
    pub(crate) refs: Vec<(BString, BString)>,
    pub(crate) actions: Vec<Action>,
}

pub(crate) fn plan_head_update(live_view: &View, target_commit: GitOid) -> Result<HeadPlan> {
    let trimmed_head = trim_newline(live_view.head.as_slice());
    let oid_line = BString::from(format!("{target_commit}\n"));
    let (packed, mut loose) = split_refs(&live_view.refs);

    if let Some(target_ref_bytes) = trimmed_head.strip_prefix(b"ref: ") {
        let mut ref_name = BString::from(target_ref_bytes);
        let mut resolved = false;
        for _ in 0..32 {
            if let Some(val) = loose.get(&ref_name) {
                let trimmed = trim_newline(val.as_slice());
                if let Some(next_ref) = trimmed.strip_prefix(b"ref: ") {
                    ref_name = BString::from(next_ref);
                    continue;
                }
            }
            resolved = true;
            break;
        }
        if !resolved {
            return Err(Error::UnsupportedRepository(
                "symbolic reference exceeds maximum indirection depth".to_owned(),
            ));
        }
        let expected_before = loose.get(&ref_name).cloned();
        let mut actions = Vec::new();
        if expected_before.as_ref() != Some(&oid_line) {
            actions.push(Action::WriteRef {
                name: ref_name.clone(),
                expected_before,
                target: oid_line.clone(),
            });
        }
        loose.insert(ref_name, oid_line);
        Ok(HeadPlan {
            head: live_view.head.clone(),
            refs: rebuild_refs(packed, loose),
            actions,
        })
    } else {
        let expected_before = live_view.head.clone();
        let mut actions = Vec::new();
        if expected_before != oid_line {
            actions.push(Action::WriteHead {
                expected_before,
                target: oid_line.clone(),
            });
        }
        Ok(HeadPlan {
            head: oid_line,
            refs: rebuild_refs(packed, loose),
            actions,
        })
    }
}

fn rebuild_refs(
    packed: Option<BString>,
    loose: BTreeMap<BString, BString>,
) -> Vec<(BString, BString)> {
    let mut out: Vec<(BString, BString)> = loose.into_iter().collect();
    if let Some(packed_bytes) = packed {
        out.push((BString::from("packed-refs"), packed_bytes));
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    out
}

struct PreparedReset {
    target_index_bytes: Option<Vec<u8>>,
    target_files: Vec<FileRecord>,
    cas_writes: Vec<Vec<u8>>,
    worktree_and_index_actions: Vec<Action>,
}

#[allow(clippy::too_many_arguments)]
fn prepare_reset_state(
    repo: &LocalRepository,
    store: &RecoveryStore,
    reader: &mut BoundedReader,
    mode: ResetMode,
    tree_oid: GitOid,
    live_view: &View,
    live_index: Option<&gix::index::State>,
    policy: CapturePolicy,
) -> Result<PreparedReset> {
    if mode == ResetMode::Soft {
        let target_index_bytes = live_view
            .index
            .map(|id| store.read_content_bounded(id, MAX_CAS_BYTES))
            .transpose()?;
        return Ok(PreparedReset {
            target_index_bytes,
            target_files: live_view.files.clone(),
            cas_writes: Vec::new(),
            worktree_and_index_actions: Vec::new(),
        });
    }

    let mut tree_entries = BTreeMap::new();
    flatten_tree(reader, tree_oid, b"", &mut tree_entries)?;
    let new_index_bytes = build_index_bytes(&tree_entries)?;
    let new_index = decode_index(&new_index_bytes)?;
    validate_index(&new_index)?;
    let new_index_id = ContentId::for_bytes(&new_index_bytes);

    let mut actions = Vec::new();
    let mut cas_writes = Vec::new();
    if live_view.index != Some(new_index_id) {
        cas_writes.push(new_index_bytes.clone());
    }

    let excludes_file = repo.global_excludes_file()?;
    let case = if repo.ignore_case() {
        Case::Fold
    } else {
        Case::Sensitive
    };
    let live_map = file_map(live_view);

    let (target_files, mut extra_blobs) = if mode == ResetMode::Mixed {
        prepare_mixed_files(
            repo,
            excludes_file,
            case,
            live_index,
            &new_index,
            &live_map,
            &tree_entries,
            policy,
        )?
    } else {
        prepare_hard_reset(
            repo,
            reader,
            excludes_file,
            case,
            live_index,
            &new_index,
            &live_map,
            &tree_entries,
            policy,
            &mut actions,
        )?
    };
    cas_writes.append(&mut extra_blobs);

    if live_view.index != Some(new_index_id) {
        actions.push(Action::WriteIndex {
            expected_before: live_view.index,
            target: new_index_id,
        });
    }

    Ok(PreparedReset {
        target_index_bytes: Some(new_index_bytes),
        target_files,
        cas_writes,
        worktree_and_index_actions: actions,
    })
}

#[allow(clippy::too_many_arguments)]
fn prepare_mixed_files(
    repo: &LocalRepository,
    excludes_file: Option<std::path::PathBuf>,
    case: Case,
    live_index: Option<&gix::index::State>,
    new_index: &gix::index::State,
    live_map: &BTreeMap<BString, EntryState>,
    tree_entries: &BTreeMap<BString, (FileKind, GitOid)>,
    policy: CapturePolicy,
) -> Result<(Vec<FileRecord>, Vec<Vec<u8>>)> {
    let worktree = CheckedRoot::open(repo.workdir())?;
    let target_profile = IgnoreProfile::for_live(
        repo.git_dir(),
        repo.workdir(),
        &worktree,
        excludes_file,
        case,
        Some(new_index),
        live_index,
        policy,
    )?;

    let mut target_files_map: BTreeMap<BString, FileRecord> = BTreeMap::new();
    let mut cas_writes = Vec::new();
    for (path, state) in live_map {
        if !target_profile.is_ignored(path.as_slice(), false) {
            target_files_map.insert(
                path.clone(),
                FileRecord {
                    path: path.clone(),
                    kind: state.kind,
                    content: state.content,
                },
            );
        }
    }
    for path in tree_entries.keys() {
        if target_files_map.contains_key(path) {
            continue;
        }
        if let Some(captured) = worktree.read_entry(path.as_slice(), policy.max_file_bytes)? {
            let content = ContentId::for_bytes(&captured.bytes);
            cas_writes.push(captured.bytes);
            target_files_map.insert(
                path.clone(),
                FileRecord {
                    path: path.clone(),
                    kind: captured.kind,
                    content,
                },
            );
        }
    }
    Ok((target_files_map.into_values().collect(), cas_writes))
}

#[allow(clippy::too_many_arguments)]
fn prepare_hard_reset(
    repo: &LocalRepository,
    reader: &mut BoundedReader,
    excludes_file: Option<std::path::PathBuf>,
    case: Case,
    live_index: Option<&gix::index::State>,
    new_index: &gix::index::State,
    live_map: &BTreeMap<BString, EntryState>,
    tree_entries: &BTreeMap<BString, (FileKind, GitOid)>,
    policy: CapturePolicy,
    actions: &mut Vec<Action>,
) -> Result<(Vec<FileRecord>, Vec<Vec<u8>>)> {
    let mut filter = RepoFilter::new_smudge(repo, new_index)?;
    let (target_blobs, mut cas_writes, mut gitignores) =
        collect_hard_target_blobs(reader, &mut filter, live_map, tree_entries, policy)?;

    let worktree = CheckedRoot::open(repo.workdir())?;
    let current_profile = IgnoreProfile::for_live(
        repo.git_dir(),
        repo.workdir(),
        &worktree,
        excludes_file.clone(),
        case,
        live_index,
        Some(new_index),
        policy,
    )?;

    let live_tracked = tracked_set(live_index);
    let target_file_set: BTreeSet<&[u8]> = target_blobs.keys().map(|p| p.as_slice()).collect();
    let target_dirs = ancestor_dir_set(target_blobs.keys().map(|p| p.as_slice()));
    for path in live_map.keys() {
        if !live_tracked.contains(path)
            && !target_blobs.contains_key(path)
            && !target_dirs.contains(path)
            && !has_ancestor_in(path.as_slice(), &target_file_set)
            && is_gitignore_path(path.as_slice())
            && let Some(captured) = worktree.read_entry(path.as_slice(), policy.max_file_bytes)?
            && captured.kind != FileKind::Symlink
        {
            gitignores.push((path.clone(), captured.bytes));
        }
    }

    let target_profile = IgnoreProfile::from_gitignore_blobs(
        repo.git_dir(),
        repo.workdir(),
        excludes_file,
        case,
        Some(new_index),
        gitignores,
    )?;

    let scan = inspect_hard_worktree(
        &worktree,
        policy,
        live_map,
        &live_tracked,
        &target_blobs,
        &target_dirs,
        &target_file_set,
        &current_profile,
        &target_profile,
    )?;

    append_hard_worktree_actions(
        actions,
        live_map,
        &live_tracked,
        &target_blobs,
        &target_dirs,
        &target_file_set,
        &target_profile,
        &scan,
    );

    let mut target_files_map: BTreeMap<BString, FileRecord> = BTreeMap::new();
    for (path, (state, _)) in &target_blobs {
        target_files_map.insert(
            path.clone(),
            FileRecord {
                path: path.clone(),
                kind: state.kind,
                content: state.content,
            },
        );
    }
    for (path, state) in &scan.preserved_nonignored {
        target_files_map.insert(
            path.clone(),
            FileRecord {
                path: path.clone(),
                kind: state.kind,
                content: state.content,
            },
        );
    }
    for (path, state, bytes) in scan.newly_exposed_ignored {
        cas_writes.push(bytes);
        target_files_map.insert(
            path.clone(),
            FileRecord {
                path,
                kind: state.kind,
                content: state.content,
            },
        );
    }
    Ok((target_files_map.into_values().collect(), cas_writes))
}

#[allow(clippy::type_complexity)]
fn collect_hard_target_blobs(
    reader: &mut BoundedReader,
    filter: &mut RepoFilter<'_>,
    live_map: &BTreeMap<BString, EntryState>,
    tree_entries: &BTreeMap<BString, (FileKind, GitOid)>,
    policy: CapturePolicy,
) -> Result<(
    BTreeMap<BString, (EntryState, Vec<u8>)>,
    Vec<Vec<u8>>,
    Vec<(BString, Vec<u8>)>,
)> {
    let max_file = usize::try_from(policy.max_file_bytes)
        .unwrap_or(usize::MAX)
        .min(MAX_CAS_BYTES);
    let mut target_blobs: BTreeMap<BString, (EntryState, Vec<u8>)> = BTreeMap::new();
    let mut cas_writes = Vec::new();
    let mut gitignores = Vec::new();
    for (path, (kind, oid)) in tree_entries {
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
        target_blobs.insert(path.clone(), (state, worktree_bytes));
    }
    Ok((target_blobs, cas_writes, gitignores))
}

struct HardWorktreeScan {
    existing_dirs: BTreeSet<BString>,
    dirs_with_kept_entries: BTreeSet<BString>,
    reclassified_keep: BTreeSet<BString>,
    preserved_nonignored: BTreeMap<BString, EntryState>,
    newly_exposed_ignored: Vec<(BString, EntryState, Vec<u8>)>,
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn inspect_hard_worktree(
    worktree: &CheckedRoot,
    policy: CapturePolicy,
    live_map: &BTreeMap<BString, EntryState>,
    live_tracked: &BTreeSet<BString>,
    target_blobs: &BTreeMap<BString, (EntryState, Vec<u8>)>,
    target_dirs: &BTreeSet<BString>,
    target_file_set: &BTreeSet<&[u8]>,
    current_profile: &IgnoreProfile,
    target_profile: &IgnoreProfile,
) -> Result<HardWorktreeScan> {
    let live_dirs = ancestor_dir_set(live_map.keys().map(|p| p.as_slice()));
    let mut scan = HardWorktreeScan {
        existing_dirs: BTreeSet::new(),
        dirs_with_kept_entries: BTreeSet::new(),
        reclassified_keep: BTreeSet::new(),
        preserved_nonignored: BTreeMap::new(),
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
            let under_target_file = has_ancestor_in(rel.as_slice(), target_file_set);
            match kind {
                PathEntryKind::Directory => {
                    if name.as_slice() == b".git" && !dir.is_empty() {
                        check_nested_git_dir(
                            dir.as_slice(),
                            under_target_file,
                            target_dirs.contains(&dir),
                            target_profile,
                            "reset",
                        )?;
                        continue;
                    }
                    scan.existing_dirs.insert(rel.clone());
                    let must_descend = target_blobs.contains_key(&rel)
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
                        || target_blobs.contains_key(&rel)
                        || target_dirs.contains(&rel)
                        || live_map.contains_key(&rel)
                        || !current_profile.is_ignored(rel.as_slice(), false)
                        || !target_profile.is_ignored(rel.as_slice(), false)
                    {
                        return Err(Error::UnsupportedRepository(format!(
                            "unsupported worktree entry at {} blocks reset",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                }
                PathEntryKind::Regular | PathEntryKind::Executable | PathEntryKind::Symlink => {
                    if live_map.contains_key(&rel) {
                        if !live_tracked.contains(&rel)
                            && !target_blobs.contains_key(&rel)
                            && !target_dirs.contains(&rel)
                            && !under_target_file
                        {
                            // Unrelated non-ignored untracked file: preserve on disk.
                            if !target_profile.is_ignored(rel.as_slice(), false) {
                                scan.preserved_nonignored
                                    .insert(rel.clone(), live_map[&rel]);
                            }
                            mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                        }
                        continue;
                    }
                    // On-disk ignored entry (absent from live_map):
                    if under_target_file {
                        return Err(Error::UnsupportedRepository(format!(
                            "ignored entry {} blocks directory replacement",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                    if let Some((target_state, _)) = target_blobs.get(&rel) {
                        let captured = worktree
                            .read_entry(rel.as_slice(), policy.max_file_bytes)?
                            .ok_or_else(|| {
                                Error::UnsupportedRepository(format!(
                                    "entry {} disappeared during reset planning",
                                    escape_path_bytes(rel.as_slice())
                                ))
                            })?;
                        let disk_state = EntryState {
                            kind: captured.kind,
                            content: ContentId::for_bytes(&captured.bytes),
                        };
                        if disk_state == *target_state {
                            scan.reclassified_keep.insert(rel);
                        } else {
                            return Err(Error::UnsupportedRepository(format!(
                                "ignored entry {} blocks target path",
                                escape_path_bytes(rel.as_slice())
                            )));
                        }
                    } else if target_dirs.contains(&rel) {
                        return Err(Error::UnsupportedRepository(format!(
                            "ignored entry {} blocks target directory",
                            escape_path_bytes(rel.as_slice())
                        )));
                    } else if !target_profile.is_ignored(rel.as_slice(), false) {
                        let captured = worktree
                            .read_entry(rel.as_slice(), policy.max_file_bytes)?
                            .ok_or_else(|| {
                                Error::UnsupportedRepository(format!(
                                    "entry {} disappeared during reset planning",
                                    escape_path_bytes(rel.as_slice())
                                ))
                            })?;
                        let disk_state = EntryState {
                            kind: captured.kind,
                            content: ContentId::for_bytes(&captured.bytes),
                        };
                        scan.newly_exposed_ignored
                            .push((rel.clone(), disk_state, captured.bytes));
                        mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                    } else {
                        mark_ancestors(rel.as_slice(), &mut scan.dirs_with_kept_entries);
                    }
                }
            }
        }
    }
    Ok(scan)
}

#[allow(clippy::too_many_arguments)]
fn append_hard_worktree_actions(
    actions: &mut Vec<Action>,
    live_map: &BTreeMap<BString, EntryState>,
    live_tracked: &BTreeSet<BString>,
    target_blobs: &BTreeMap<BString, (EntryState, Vec<u8>)>,
    target_dirs: &BTreeSet<BString>,
    target_file_set: &BTreeSet<&[u8]>,
    target_profile: &IgnoreProfile,
    scan: &HardWorktreeScan,
) {
    let mut dirs_with_kept = scan.dirs_with_kept_entries.clone();
    let mut removed_paths = BTreeSet::new();
    let mut remove_actions = Vec::new();
    for (path, before) in live_map {
        if target_blobs.contains_key(path) {
            continue;
        }
        let is_untracked = !live_tracked.contains(path);
        let collides_with_target =
            target_dirs.contains(path) || has_ancestor_in(path.as_slice(), target_file_set);
        if is_untracked && !collides_with_target {
            if target_profile.is_ignored(path.as_slice(), false) {
                mark_ancestors(path.as_slice(), &mut dirs_with_kept);
            }
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

    push_dir_lifecycle_actions(
        actions,
        &scan.existing_dirs,
        target_dirs,
        target_file_set,
        &dirs_with_kept,
        &removed_paths,
    );

    for (path, (target, _)) in target_blobs {
        if scan.reclassified_keep.contains(path) || live_map.get(path) == Some(target) {
            continue;
        }
        actions.push(Action::WriteWorktreeEntry {
            path: path.clone(),
            expected_before: live_map.get(path).copied(),
            target: *target,
        });
    }
}

fn flatten_tree(
    reader: &mut BoundedReader,
    tree_oid: GitOid,
    prefix: &[u8],
    out: &mut BTreeMap<BString, (FileKind, GitOid)>,
) -> Result<()> {
    let obj = reader.read(tree_oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::CorruptObject(format!(
            "object {tree_oid} is a {}, expected tree",
            obj.kind
        )));
    }
    let tree = Tree::parse(&obj.data)?;
    for entry in tree.entries {
        if entry.name.as_slice() == b".git"
            || entry.name.as_slice() == b"."
            || entry.name.as_slice() == b".."
            || entry.name.as_slice().contains(&b'/')
            || entry.name.as_slice().contains(&0)
        {
            return Err(Error::UnsupportedRepository(format!(
                "tree {tree_oid} contains unsafe component {}",
                escape_path_bytes(entry.name.as_slice())
            )));
        }
        let rel = join_rel(prefix, entry.name.as_slice());
        match entry.mode {
            TreeMode::Gitlink => {
                return Err(Error::UnsupportedRepository(format!(
                    "submodules are not supported ({})",
                    escape_path_bytes(rel.as_slice())
                )));
            }
            TreeMode::Directory => flatten_tree(reader, entry.oid, rel.as_slice(), out)?,
            TreeMode::File => {
                if out
                    .insert(rel.clone(), (FileKind::Regular, entry.oid))
                    .is_some()
                {
                    return Err(Error::CorruptObject(format!(
                        "duplicate tree path {}",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
            }
            TreeMode::Executable => {
                if out
                    .insert(rel.clone(), (FileKind::Executable, entry.oid))
                    .is_some()
                {
                    return Err(Error::CorruptObject(format!(
                        "duplicate tree path {}",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
            }
            TreeMode::Symlink => {
                if out
                    .insert(rel.clone(), (FileKind::Symlink, entry.oid))
                    .is_some()
                {
                    return Err(Error::CorruptObject(format!(
                        "duplicate tree path {}",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
            }
        }
    }
    Ok(())
}

fn build_index_bytes(entries: &BTreeMap<BString, (FileKind, GitOid)>) -> Result<Vec<u8>> {
    let mut state = gix::index::State::new(gix::hash::Kind::Sha1);
    for (path, (kind, oid)) in entries {
        let mode = match kind {
            FileKind::Regular => gix::index::entry::Mode::FILE,
            FileKind::Executable => gix::index::entry::Mode::FILE_EXECUTABLE,
            FileKind::Symlink => gix::index::entry::Mode::SYMLINK,
        };
        let gix_oid = gix::ObjectId::from_bytes_or_panic(&oid.as_bytes());
        state.dangerously_push_entry(
            gix::index::entry::Stat::default(),
            gix_oid,
            gix::index::entry::Flags::empty(),
            mode,
            path.as_bstr(),
        );
    }
    serialize_index(&mut state)
}

pub(crate) fn serialize_index(state: &mut gix::index::State) -> Result<Vec<u8>> {
    state.sort_entries();

    let mut out = Vec::new();
    let options = gix::index::write::Options {
        extensions: gix::index::write::Extensions::None,
        skip_hash: false,
    };
    state
        .write_to(&mut out, options)
        .map_err(|error| Error::Gix(error.to_string()))?;
    let digest = Sha1::digest(&out);
    out.extend_from_slice(digest.as_slice());
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn persist_target_view(
    store: &RecoveryStore,
    objects: &ObjectStore,
    known_cas: &BTreeSet<ContentId>,
    head: BString,
    refs: Vec<(BString, BString)>,
    index_bytes: Option<&[u8]>,
    mut files: Vec<FileRecord>,
    cas_writes: Vec<Vec<u8>>,
) -> Result<ContentId> {
    files.sort_by(|left, right| left.path.cmp(&right.path));
    for payload in cas_writes {
        retention::persist_cas_with_known(store, &payload, known_cas)?;
    }
    let index = index_bytes
        .map(|bytes| retention::persist_cas_with_known(store, bytes, known_cas))
        .transpose()?;
    let decoded_index = index_bytes.map(decode_index).transpose()?;
    let roots = retention::roots(head.as_slice(), &refs, decoded_index.as_ref())?;
    check_view_size(
        head.len(),
        &refs,
        index.is_some(),
        files.iter().map(|file| file.path.len()),
        roots.len(),
    )?;
    let retained_objects = retention::retain_roots_with_known(store, objects, roots, known_cas)?;
    check_view_size(
        head.len(),
        &refs,
        index.is_some(),
        files.iter().map(|file| file.path.len()),
        retained_objects.len(),
    )?;
    let view = View {
        head,
        refs,
        index,
        files,
        retained_objects,
    };
    store.put_view(&view)
}

pub(crate) fn trim_newline(mut bytes: &[u8]) -> &[u8] {
    while bytes.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}
