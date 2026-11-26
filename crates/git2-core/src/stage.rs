//! Native `git2 add [--all] [--] [<path>...]` staging with built-in EOL filtering.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bstr::{BString, ByteSlice};
use gix::attrs::StateRef;
use gix::ignore::glob::pattern::Case;
use gix::prelude::Find;

use crate::object::{GitOid, ObjectKind, RawObject, hash_object};
use crate::paths::{CheckedRoot, PathEntryKind, checked_split};
use crate::recovery::{CapturePolicy, ContentId, FileKind, RecoveryStore, View, check_view_size};
use crate::repository::{LocalRepository, decode_index, known_cas_for_view, validate_index};
use crate::reset::serialize_index;
use crate::restore::{
    IgnoreProfile, MAX_SYMLINK_TARGET_BYTES, ancestor_dir_set, decode_view_index_readonly,
    ensure_no_foreign_locks, has_ancestor_in, join_rel, tracked_set,
};
use crate::retention::{self, MAX_OBJECT_BYTES};
use crate::store::{BoundedReader, ObjectStore};
use crate::transaction::{self, Action, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

const MAX_COMMAND_BYTES: usize = 4096;

/// Summary returned after a durable `git2 add`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddOutcome {
    pub operation_id: ContentId,
    pub after_view: ContentId,
    pub staged_count: usize,
    pub removed_count: usize,
}

/// Stage literal files/directories (including deletions) or `--all` across the repository.
///
/// # Errors
///
/// Returns an error if a pending transaction exists, no paths/`--all` are given, an explicit
/// literal path is missing, ignored, escapes the repository, or triggers an unsupported active
/// external filter/encoding, or if durable execution fails.
pub fn apply(
    repo: &LocalRepository,
    cwd: &Path,
    all: bool,
    paths: &[OsString],
) -> Result<AddOutcome> {
    apply_with_hook(repo, cwd, all, paths, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_with_hook(
    repo: &LocalRepository,
    cwd: &Path,
    all: bool,
    paths: &[OsString],
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<AddOutcome> {
    if !all && paths.is_empty() {
        return Err(Error::UnsupportedRepository(
            "nothing specified, nothing added; provide literal paths or pass --all".to_owned(),
        ));
    }
    if all && !paths.is_empty() {
        return Err(Error::UnsupportedRepository(
            "--all cannot be combined with explicit paths".to_owned(),
        ));
    }

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
    let base_index = live_index
        .clone()
        .unwrap_or_else(|| gix::index::State::new(gix::hash::Kind::Sha1));
    let targets = scan_stage_targets(repo, cwd, all, paths, &base_index, policy)?;

    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let mut filter_ctx = RepoFilter::new_clean(repo, &base_index)?;
    let worktree = CheckedRoot::open(repo.workdir())?;
    let staged_entries = convert_and_prepare_blobs(
        &worktree,
        &mut reader,
        &base_index,
        &mut filter_ctx,
        &targets.staged,
        policy,
    )?;

    let staged_ancestors = ancestor_dir_set(staged_entries.keys().map(|p| p.as_slice()));
    let staged_set: BTreeSet<&[u8]> = staged_entries.keys().map(|p| p.as_slice()).collect();
    let index_differs = index_entries_differ(
        &base_index,
        &targets.removed,
        &staged_entries,
        &staged_ancestors,
        &staged_set,
    );
    let new_blobs = collect_missing_blobs(&staged_entries);
    if !index_differs && live_view.index.is_some() && new_blobs.is_empty() {
        return Ok(AddOutcome {
            operation_id: expected_head,
            after_view: live_view_id,
            staged_count: 0,
            removed_count: 0,
        });
    }

    let new_index_bytes = rebuild_staged_index(
        &base_index,
        &targets.removed,
        &staged_entries,
        &staged_ancestors,
        &staged_set,
    )?;
    let decoded_new = decode_index(&new_index_bytes)?;
    validate_index(&decoded_new)?;
    let new_index_id = ContentId::for_bytes(&new_index_bytes);

    let actions = build_stage_actions(live_view.index, new_index_id, &new_blobs);
    ensure_no_foreign_locks(repo.git_dir(), &actions)?;

    let after_view = if actions.is_empty() {
        live_view_id
    } else {
        persist_stage_view(
            &store,
            &objects,
            &live_view,
            &new_index_bytes,
            &decoded_new,
            &new_blobs,
        )?
    };

    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: after_view,
        command: format_add_command(all, paths),
        changes: MutationPlan::changes_for_actions(&actions),
        actions,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(AddOutcome {
        operation_id,
        after_view,
        staged_count: staged_entries.len(),
        removed_count: targets.removed.len(),
    })
}

fn scan_stage_targets(
    repo: &LocalRepository,
    cwd: &Path,
    all: bool,
    paths: &[OsString],
    base_index: &gix::index::State,
    policy: CapturePolicy,
) -> Result<StageTargets> {
    let live_tracked = tracked_set(Some(base_index));
    let worktree = CheckedRoot::open(repo.workdir())?;
    let excludes_file = repo.global_excludes_file()?;
    let case = if repo.ignore_case() {
        Case::Fold
    } else {
        Case::Sensitive
    };
    let ignore_profile = IgnoreProfile::for_live(
        repo.git_dir(),
        repo.workdir(),
        &worktree,
        excludes_file,
        case,
        Some(base_index),
        None,
        policy,
    )?;
    let normalized_targets = if all {
        Vec::new()
    } else {
        resolve_literal_args(repo.workdir(), cwd, paths)?
    };
    collect_stage_targets(
        &worktree,
        &ignore_profile,
        &live_tracked,
        all,
        &normalized_targets,
    )
}

fn resolve_literal_args(workdir: &Path, cwd: &Path, paths: &[OsString]) -> Result<Vec<BString>> {
    let canonical_root = workdir.canonicalize()?;
    let canonical_cwd = cwd.canonicalize()?;
    let cwd_rel = canonical_cwd.strip_prefix(&canonical_root).map_err(|_| {
        Error::UnsupportedRepository("current directory is outside repository worktree".to_owned())
    })?;
    let cwd_components: Vec<&[u8]> = cwd_rel
        .as_os_str()
        .as_bytes()
        .split(|&b| b == b'/')
        .filter(|part| !part.is_empty())
        .collect();

    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        let bytes = raw.as_bytes();
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(Error::UnsupportedRepository(format!(
                "invalid empty or NUL path argument {}",
                escape_path_bytes(bytes)
            )));
        }
        let rel = normalize_literal_path(workdir, &canonical_root, &cwd_components, bytes)?;
        if rel.as_slice() == b".git" || rel.starts_with(b".git/") {
            return Err(Error::UnsupportedRepository(format!(
                "refusing to stage internal Git metadata path {}",
                escape_path_bytes(rel.as_slice())
            )));
        }
        out.push(rel);
    }
    Ok(out)
}

fn normalize_literal_path(
    workdir: &Path,
    canonical_root: &Path,
    cwd_components: &[&[u8]],
    raw: &[u8],
) -> Result<BString> {
    let mut stack: Vec<Vec<u8>> = if raw.starts_with(b"/") {
        let stripped = strip_workdir_prefix(raw, workdir.as_os_str().as_bytes())
            .or_else(|| strip_workdir_prefix(raw, canonical_root.as_os_str().as_bytes()))
            .ok_or_else(|| {
                Error::UnsupportedRepository(format!(
                    "path {} is outside repository worktree",
                    escape_path_bytes(raw)
                ))
            })?;
        let mut init = Vec::new();
        for comp in stripped.split(|&b| b == b'/') {
            if !comp.is_empty() {
                push_normalized(&mut init, comp, raw)?;
            }
        }
        return Ok(join_components(&init));
    } else {
        cwd_components.iter().map(|c| c.to_vec()).collect()
    };

    for comp in raw.split(|&b| b == b'/') {
        if comp.is_empty() || comp == b"." {
            continue;
        }
        push_normalized(&mut stack, comp, raw)?;
    }
    Ok(join_components(&stack))
}

fn strip_workdir_prefix<'a>(path: &'a [u8], root: &[u8]) -> Option<&'a [u8]> {
    let trimmed_root = root.strip_suffix(b"/").unwrap_or(root);
    let rest = path.strip_prefix(trimmed_root)?;
    if rest.is_empty() {
        Some(b"")
    } else {
        rest.strip_prefix(b"/")
    }
}

fn push_normalized(stack: &mut Vec<Vec<u8>>, comp: &[u8], raw: &[u8]) -> Result<()> {
    if comp == b"." || comp.is_empty() {
        return Ok(());
    }
    if comp == b".." {
        if stack.pop().is_none() {
            return Err(Error::UnsupportedRepository(format!(
                "path {} escapes repository worktree",
                escape_path_bytes(raw)
            )));
        }
        return Ok(());
    }
    stack.push(comp.to_vec());
    Ok(())
}

fn join_components(components: &[Vec<u8>]) -> BString {
    if components.is_empty() {
        return BString::default();
    }
    let total_len = components.iter().map(Vec::len).sum::<usize>() + components.len() - 1;
    let mut out = Vec::with_capacity(total_len);
    for (idx, comp) in components.iter().enumerate() {
        if idx > 0 {
            out.push(b'/');
        }
        out.extend_from_slice(comp);
    }
    BString::from(out)
}

struct StageTargets {
    staged: BTreeSet<BString>,
    removed: BTreeSet<BString>,
}

fn stage_whole_worktree(
    worktree: &CheckedRoot,
    ignore_profile: &IgnoreProfile,
    live_tracked: &BTreeSet<BString>,
    out: &mut StageTargets,
) -> Result<()> {
    walk_directory_stage(worktree, ignore_profile, live_tracked, b"", &mut out.staged)?;
    for tracked in live_tracked {
        if entry_kind_or_absent(worktree, tracked.as_slice())?.is_none() {
            out.removed.insert(tracked.clone());
        }
    }
    Ok(())
}

fn collect_stage_targets(
    worktree: &CheckedRoot,
    ignore_profile: &IgnoreProfile,
    live_tracked: &BTreeSet<BString>,
    all: bool,
    targets: &[BString],
) -> Result<StageTargets> {
    let mut out = StageTargets {
        staged: BTreeSet::new(),
        removed: BTreeSet::new(),
    };
    if all {
        stage_whole_worktree(worktree, ignore_profile, live_tracked, &mut out)?;
        return Ok(out);
    }

    for rel in targets {
        if rel.is_empty() {
            stage_whole_worktree(worktree, ignore_profile, live_tracked, &mut out)?;
            continue;
        }
        match entry_kind_or_absent(worktree, rel.as_slice())? {
            Some(PathEntryKind::Directory) => {
                let prefix = dir_prefix(rel.as_slice());
                let has_tracked_under = has_tracked_prefix(live_tracked, prefix.as_slice());
                if ignore_profile.is_ignored(rel.as_slice(), true) && !has_tracked_under {
                    return Err(Error::UnsupportedRepository(format!(
                        "directory {} is ignored; forced addition is not supported",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
                walk_directory_stage(
                    worktree,
                    ignore_profile,
                    live_tracked,
                    rel.as_slice(),
                    &mut out.staged,
                )?;
                for tracked in tracked_with_prefix(live_tracked, prefix.as_slice()) {
                    if entry_kind_or_absent(worktree, tracked.as_slice())?.is_none() {
                        out.removed.insert(tracked.clone());
                    }
                }
            }
            Some(PathEntryKind::Regular | PathEntryKind::Executable | PathEntryKind::Symlink) => {
                if !live_tracked.contains(rel) && ignore_profile.is_ignored(rel.as_slice(), false) {
                    return Err(Error::UnsupportedRepository(format!(
                        "path {} is ignored; forced addition is not supported",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
                out.staged.insert(rel.clone());
                let prefix = dir_prefix(rel.as_slice());
                for tracked in tracked_with_prefix(live_tracked, prefix.as_slice()) {
                    out.removed.insert(tracked.clone());
                }
            }
            Some(PathEntryKind::Other) => {
                return Err(Error::UnsupportedRepository(format!(
                    "unsupported special entry at {}",
                    escape_path_bytes(rel.as_slice())
                )));
            }
            None => {
                let prefix = dir_prefix(rel.as_slice());
                let mut matched = false;
                if live_tracked.contains(rel) {
                    out.removed.insert(rel.clone());
                    matched = true;
                }
                for tracked in tracked_with_prefix(live_tracked, prefix.as_slice()) {
                    out.removed.insert(tracked.clone());
                    matched = true;
                }
                if !matched {
                    return Err(Error::UnsupportedRepository(format!(
                        "pathspec {} did not match any files",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
            }
        }
    }
    Ok(out)
}

fn has_tracked_prefix(live_tracked: &BTreeSet<BString>, prefix: &[u8]) -> bool {
    let start = BString::from(prefix);
    live_tracked
        .range(start..)
        .next()
        .is_some_and(|path| path.starts_with(prefix))
}

fn tracked_with_prefix<'a>(
    live_tracked: &'a BTreeSet<BString>,
    prefix: &'a [u8],
) -> impl Iterator<Item = &'a BString> {
    let start = BString::from(prefix);
    live_tracked
        .range(start..)
        .take_while(move |path| path.starts_with(prefix))
}

fn walk_directory_stage(
    worktree: &CheckedRoot,
    ignore_profile: &IgnoreProfile,
    live_tracked: &BTreeSet<BString>,
    root_dir: &[u8],
    staged: &mut BTreeSet<BString>,
) -> Result<()> {
    let mut queue = VecDeque::new();
    queue.push_back(BString::from(root_dir));
    while let Some(dir) = queue.pop_front() {
        for (name, kind) in worktree.list_dir_entries(dir.as_slice())? {
            if dir.is_empty() && name.as_slice() == b".git" {
                continue;
            }
            let rel = join_rel(dir.as_slice(), name.as_slice());
            match kind {
                PathEntryKind::Directory => {
                    if name.as_slice() == b".git" && !dir.is_empty() {
                        return Err(Error::UnsupportedRepository(format!(
                            "nested repository at {} cannot be staged",
                            escape_path_bytes(dir.as_slice())
                        )));
                    }
                    let prefix = dir_prefix(rel.as_slice());
                    let has_tracked_child = has_tracked_prefix(live_tracked, prefix.as_slice());
                    if !ignore_profile.is_ignored(rel.as_slice(), true) || has_tracked_child {
                        queue.push_back(rel);
                    }
                }
                PathEntryKind::Other => {
                    if !ignore_profile.is_ignored(rel.as_slice(), false) {
                        return Err(Error::UnsupportedRepository(format!(
                            "unsupported special entry at {}",
                            escape_path_bytes(rel.as_slice())
                        )));
                    }
                }
                PathEntryKind::Regular | PathEntryKind::Executable | PathEntryKind::Symlink => {
                    if live_tracked.contains(&rel)
                        || !ignore_profile.is_ignored(rel.as_slice(), false)
                    {
                        staged.insert(rel);
                    }
                }
            }
        }
    }
    Ok(())
}

fn entry_kind_or_absent(worktree: &CheckedRoot, path: &[u8]) -> Result<Option<PathEntryKind>> {
    match worktree.entry_kind(path) {
        Ok(kind) => Ok(kind),
        Err(Error::Io(error))
            if error.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error()) =>
        {
            Ok(None)
        }
        Err(Error::UnsupportedRepository(message))
            if message.contains("has a non-directory parent") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub(crate) struct RepoFilter<'repo> {
    repo: &'repo gix::Repository,
    pipeline: gix::filter::plumbing::Pipeline,
    attr_stack: gix::worktree::Stack,
    active_drivers: BTreeSet<BString>,
    op_label: &'static str,
}

impl<'repo> RepoFilter<'repo> {
    pub(crate) fn new_clean(
        repo: &'repo LocalRepository,
        index: &gix::index::State,
    ) -> Result<Self> {
        Self::new_inner(
            repo,
            index,
            gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
            false,
        )
    }

    pub(crate) fn new_smudge(
        repo: &'repo LocalRepository,
        index: &gix::index::State,
    ) -> Result<Self> {
        Self::new_inner(
            repo,
            index,
            gix::worktree::stack::state::attributes::Source::IdMapping,
            true,
        )
    }

    fn new_inner(
        repo: &'repo LocalRepository,
        index: &gix::index::State,
        attr_source: gix::worktree::stack::state::attributes::Source,
        smudge: bool,
    ) -> Result<Self> {
        let mut options = gix::filter::Pipeline::options(&repo.inner)
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?;
        let mut active_drivers = BTreeSet::new();
        for driver in &options.drivers {
            let cmd = if smudge {
                driver.smudge.is_some()
            } else {
                driver.clean.is_some()
            };
            if cmd || driver.process.is_some() || driver.required {
                active_drivers.insert(driver.name.clone());
            }
        }
        // Never allow `gix` to execute external filter processes.
        options.drivers.clear();

        let command_ctx = repo
            .inner
            .command_context()
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?;
        let pipeline = gix::filter::plumbing::Pipeline::new(command_ctx, options);
        let attr_stack = repo
            .inner
            .attributes_only(index, attr_source)
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?
            .detach();
        Ok(Self {
            repo: &repo.inner,
            pipeline,
            attr_stack,
            active_drivers,
            op_label: if smudge { "smudge" } else { "clean" },
        })
    }

    fn check_platform_attrs(
        active_drivers: &BTreeSet<BString>,
        op_label: &'static str,
        path: &[u8],
        check: &gix::attrs::search::Outcome,
    ) -> Result<()> {
        for matched in check.iter_selected() {
            match matched.assignment.name.as_str() {
                "filter" => {
                    if let StateRef::Value(name) = matched.assignment.state
                        && active_drivers.contains(name.as_bstr())
                    {
                        return Err(Error::UnsupportedRepository(format!(
                            "external {op_label} filter {:?} applies to {}; external filters are not supported",
                            name.as_bstr(),
                            escape_path_bytes(path)
                        )));
                    }
                }
                "working-tree-encoding" => match matched.assignment.state {
                    StateRef::Set | StateRef::Unset => {
                        return Err(Error::UnsupportedRepository(format!(
                            "invalid working-tree-encoding attribute on {}",
                            escape_path_bytes(path)
                        )));
                    }
                    StateRef::Value(enc)
                        if !enc.as_bstr().eq_ignore_ascii_case(b"UTF-8")
                            && !enc.as_bstr().eq_ignore_ascii_case(b"utf8") =>
                    {
                        return Err(Error::UnsupportedRepository(format!(
                            "working-tree-encoding {:?} on {} is not supported",
                            enc.as_bstr(),
                            escape_path_bytes(path)
                        )));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        Ok(())
    }

    fn convert_to_git(
        &mut self,
        path: &[u8],
        raw_bytes: &[u8],
        index: &gix::index::State,
    ) -> Result<Vec<u8>> {
        let rel_path = Path::new(OsStr::from_bytes(path));
        let mut check = self
            .attr_stack
            .selected_attribute_matches(["filter", "working-tree-encoding"]);
        let platform = self
            .attr_stack
            .at_path(
                rel_path,
                Some(gix::index::entry::Mode::FILE),
                &self.repo.objects,
            )
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?;
        platform.matching_attributes(&mut check);
        Self::check_platform_attrs(&self.active_drivers, self.op_label, path, &check)?;

        let outcome = self
            .pipeline
            .convert_to_git(
                io::Cursor::new(raw_bytes),
                rel_path,
                &mut |_, attrs| {
                    platform.matching_attributes(attrs);
                },
                &mut |buf| -> std::result::Result<
                    Option<()>,
                    Box<dyn std::error::Error + Send + Sync>,
                > {
                    let Some(entry) = index.entry_by_path(path.as_bstr()) else {
                        return Ok(None);
                    };
                    let obj = self.repo.objects.try_find(&entry.id, buf)?;
                    Ok(obj
                        .filter(|obj| obj.kind == gix::object::Kind::Blob)
                        .map(|_| ()))
                },
            )
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?;
        let mut converted = Vec::with_capacity(raw_bytes.len().min(MAX_OBJECT_BYTES));
        outcome
            .take(MAX_OBJECT_BYTES as u64 + 1)
            .read_to_end(&mut converted)?;
        if converted.len() > MAX_OBJECT_BYTES {
            return Err(Error::UnsupportedRepository(format!(
                "converted blob {} exceeds max Git object size",
                escape_path_bytes(path)
            )));
        }
        Ok(converted)
    }

    pub(crate) fn convert_to_worktree(
        &mut self,
        path: &[u8],
        git_bytes: &[u8],
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let rel_path = Path::new(OsStr::from_bytes(path));
        let mut check = self
            .attr_stack
            .selected_attribute_matches(["filter", "working-tree-encoding"]);
        let platform = self
            .attr_stack
            .at_path(
                rel_path,
                Some(gix::index::entry::Mode::FILE),
                &self.repo.objects,
            )
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?;
        platform.matching_attributes(&mut check);
        Self::check_platform_attrs(&self.active_drivers, self.op_label, path, &check)?;

        let outcome = self
            .pipeline
            .convert_to_worktree(
                git_bytes,
                path.as_bstr(),
                &mut |_, attrs| {
                    platform.matching_attributes(attrs);
                },
                gix::filter::plumbing::driver::apply::Delay::Forbid,
            )
            .map_err(|error| Error::UnsupportedRepository(error.to_string()))?;
        let mut converted = Vec::with_capacity(git_bytes.len().min(max_bytes));
        outcome
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut converted)?;
        if converted.len() > max_bytes {
            return Err(Error::UnsupportedRepository(format!(
                "converted worktree file {} exceeds persisted size limit",
                escape_path_bytes(path)
            )));
        }
        Ok(converted)
    }
}

struct PreparedBlob {
    kind: FileKind,
    oid: GitOid,
    missing_from_objects: Option<Vec<u8>>,
}

fn convert_and_prepare_blobs(
    worktree: &CheckedRoot,
    reader: &mut BoundedReader,
    index: &gix::index::State,
    filter: &mut RepoFilter<'_>,
    staged_paths: &BTreeSet<BString>,
    policy: CapturePolicy,
) -> Result<BTreeMap<BString, PreparedBlob>> {
    let mut prepared = BTreeMap::new();
    for path in staged_paths {
        checked_split(path.as_slice())?;
        let captured = worktree
            .read_entry(path.as_slice(), policy.max_file_bytes)?
            .ok_or_else(|| {
                Error::UnsupportedRepository(format!(
                    "path {} disappeared during staging",
                    escape_path_bytes(path.as_slice())
                ))
            })?;
        let blob_bytes = if captured.kind == FileKind::Symlink {
            if captured.bytes.len() > MAX_SYMLINK_TARGET_BYTES {
                return Err(Error::UnsupportedRepository(format!(
                    "symlink {} target exceeds maximum length",
                    escape_path_bytes(path.as_slice())
                )));
            }
            captured.bytes
        } else {
            filter.convert_to_git(path.as_slice(), &captured.bytes, index)?
        };

        let oid = hash_object(ObjectKind::Blob, &blob_bytes);
        let missing_from_objects = match reader.read(oid) {
            Ok(_) => None,
            Err(Error::ObjectNotFound(_)) => Some(blob_bytes),
            Err(error) => return Err(error),
        };
        prepared.insert(
            path.clone(),
            PreparedBlob {
                kind: captured.kind,
                oid,
                missing_from_objects,
            },
        );
    }
    Ok(prepared)
}

const fn blob_mode(kind: FileKind) -> gix::index::entry::Mode {
    match kind {
        FileKind::Regular => gix::index::entry::Mode::FILE,
        FileKind::Executable => gix::index::entry::Mode::FILE_EXECUTABLE,
        FileKind::Symlink => gix::index::entry::Mode::SYMLINK,
    }
}

fn index_entries_differ(
    base: &gix::index::State,
    removed: &BTreeSet<BString>,
    staged: &BTreeMap<BString, PreparedBlob>,
    staged_ancestors: &BTreeSet<BString>,
    staged_set: &BTreeSet<&[u8]>,
) -> bool {
    let mut base_matches = 0_usize;
    for entry in base.entries() {
        let path_bytes = entry.path(base).as_ref();
        let path = BString::from(path_bytes);
        if removed.contains(&path)
            || staged_ancestors.contains(&path)
            || has_ancestor_in(path_bytes, staged_set)
        {
            return true;
        }
        if let Some(blob) = staged.get(&path) {
            if entry.stage_raw() != 0
                || entry
                    .flags
                    .contains(gix::index::entry::Flags::INTENT_TO_ADD)
                || entry.mode != blob_mode(blob.kind)
                || entry.id.as_slice() != blob.oid.as_bytes()
            {
                return true;
            }
            base_matches += 1;
        }
    }
    base_matches != staged.len()
}

fn rebuild_staged_index(
    base: &gix::index::State,
    removed: &BTreeSet<BString>,
    staged: &BTreeMap<BString, PreparedBlob>,
    staged_ancestors: &BTreeSet<BString>,
    staged_set: &BTreeSet<&[u8]>,
) -> Result<Vec<u8>> {
    let mut preserved: BTreeMap<BString, (gix::index::entry::Stat, gix::index::entry::Flags)> =
        BTreeMap::new();
    let mut next = gix::index::State::new(gix::hash::Kind::Sha1);
    for entry in base.entries() {
        let path = BString::from(entry.path(base).to_vec());
        if removed.contains(&path)
            || staged_ancestors.contains(&path)
            || has_ancestor_in(path.as_slice(), staged_set)
        {
            continue;
        }
        if let Some(blob) = staged.get(&path) {
            if entry.stage_raw() == 0
                && !entry
                    .flags
                    .contains(gix::index::entry::Flags::INTENT_TO_ADD)
                && entry.mode == blob_mode(blob.kind)
                && entry.id.as_slice() == blob.oid.as_bytes()
            {
                preserved.insert(path, (entry.stat, entry.flags));
            }
            continue;
        }
        next.dangerously_push_entry(
            entry.stat,
            entry.id,
            entry.flags,
            entry.mode,
            path.as_bstr(),
        );
    }
    for (path, blob) in staged {
        let mode = blob_mode(blob.kind);
        let (stat, flags) = preserved.get(path).copied().unwrap_or((
            gix::index::entry::Stat::default(),
            gix::index::entry::Flags::empty(),
        ));
        let gix_oid = gix::ObjectId::from_bytes_or_panic(&blob.oid.as_bytes());
        next.dangerously_push_entry(stat, gix_oid, flags, mode, path.as_bstr());
    }
    serialize_index(&mut next)
}

type PendingBlobMap = BTreeMap<GitOid, (ContentId, Vec<u8>)>;

fn collect_missing_blobs(staged: &BTreeMap<BString, PreparedBlob>) -> PendingBlobMap {
    let mut new_blobs = BTreeMap::new();
    for blob in staged.values() {
        if let Some(bytes) = &blob.missing_from_objects
            && !new_blobs.contains_key(&blob.oid)
        {
            let raw = RawObject {
                kind: ObjectKind::Blob,
                data: bytes.clone(),
            };
            let canonical = raw.canonical_bytes();
            let content = ContentId::for_bytes(&canonical);
            new_blobs.insert(blob.oid, (content, canonical));
        }
    }
    new_blobs
}

fn build_stage_actions(
    live_index_id: Option<ContentId>,
    new_index_id: ContentId,
    new_blobs: &PendingBlobMap,
) -> Vec<Action> {
    let mut actions = Vec::with_capacity(new_blobs.len() + 1);
    for (&oid, &(content, _)) in new_blobs {
        actions.push(Action::RehydrateObject { oid, content });
    }
    if live_index_id != Some(new_index_id) {
        actions.push(Action::WriteIndex {
            expected_before: live_index_id,
            target: new_index_id,
        });
    }
    actions
}

fn persist_stage_view(
    store: &RecoveryStore,
    objects: &ObjectStore,
    live_view: &View,
    new_index_bytes: &[u8],
    decoded_index: &gix::index::State,
    new_blobs: &PendingBlobMap,
) -> Result<ContentId> {
    let known = known_cas_for_view(live_view);
    let mut staged_cas_map = BTreeMap::new();
    for (&oid, (expected_content, canonical)) in new_blobs {
        let written = retention::persist_cas_with_known(store, canonical, &known)?;
        debug_assert_eq!(written, *expected_content);
        staged_cas_map.insert(oid, written);
    }
    let index_id = retention::persist_cas_with_known(store, new_index_bytes, &known)?;
    let roots = retention::roots(
        live_view.head.as_slice(),
        &live_view.refs,
        Some(decoded_index),
    )?;
    check_view_size(
        live_view.head.len(),
        &live_view.refs,
        true,
        live_view.files.iter().map(|file| file.path.len()),
        roots.len(),
    )?;
    let retained_objects =
        retention::retain_roots_with_blobs(store, objects, roots, &staged_cas_map, &known)?;
    check_view_size(
        live_view.head.len(),
        &live_view.refs,
        true,
        live_view.files.iter().map(|file| file.path.len()),
        retained_objects.len(),
    )?;
    let view = View {
        head: live_view.head.clone(),
        refs: live_view.refs.clone(),
        index: Some(index_id),
        files: live_view.files.clone(),
        retained_objects,
    };
    store.put_view(&view)
}

fn dir_prefix(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len() + 1);
    out.extend_from_slice(path);
    out.push(b'/');
    out
}

fn format_add_command(all: bool, paths: &[OsString]) -> BString {
    if all {
        return BString::from("add --all");
    }
    let mut out = String::from("add");
    for arg in paths {
        let escaped = escape_path_bytes(arg.as_bytes());
        if out.len() + 1 + escaped.len() > MAX_COMMAND_BYTES {
            out.push_str(" ...");
            break;
        }
        out.push(' ');
        out.push_str(&escaped);
    }
    BString::from(out)
}
