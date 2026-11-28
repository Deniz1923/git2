//! Shared Git reference lookup, validation, D/F checking, and revision resolution helpers.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;

use bstr::{BString, ByteSlice};

use crate::object::{Commit, GitOid, ObjectKind, Tag};
use crate::paths::{CheckedRoot, PathEntryKind, checked_split};
use crate::recovery::{MAX_CAS_BYTES, View};
use crate::reset::{parse_tilde_suffix, resolve_ref_chain, trim_newline};
use crate::restore::split_refs;
use crate::store::BoundedReader;
use crate::transaction::escape_path_bytes;
use crate::{Error, Result};

const MAX_ANCESTOR_VISIT: usize = 1_000_000;
const MAX_TAG_PEEL_HOPS: usize = 32;
const MAX_SYMREF_HOPS: usize = 32;

/// Resolved state of one Git reference inside a captured [`View`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewRefLookup {
    pub(crate) loose_bytes: Option<BString>,
    pub(crate) packed_oid: Option<GitOid>,
    pub(crate) resolved_oid: Option<GitOid>,
}

/// Validate a short branch or tag name and return its canonical `refs/<kind>/<name>` path.
pub(crate) fn validate_short_ref_name(kind: &str, name: &str, prefix: &str) -> Result<BString> {
    if name.is_empty() {
        return Err(Error::UnsupportedRepository(format!(
            "{kind} name cannot be empty"
        )));
    }
    if name.starts_with('-') {
        return Err(Error::UnsupportedRepository(format!(
            "{kind} name cannot start with '-' ({name:?})"
        )));
    }
    if prefix == "refs/heads/" && name == "HEAD" {
        return Err(Error::UnsupportedRepository(
            "branch name \"HEAD\" is forbidden".to_owned(),
        ));
    }
    if name.as_bytes().ends_with(b".lock") || name.contains(".lock/") {
        return Err(Error::UnsupportedRepository(format!(
            "invalid {kind} name {name:?}"
        )));
    }
    let full = format!("{prefix}{name}");
    checked_split(full.as_bytes())?;
    gix::validate::reference::name(full.as_bytes().as_bstr())
        .map_err(|_| Error::UnsupportedRepository(format!("invalid {kind} name {name:?}")))?;
    Ok(BString::from(full))
}

/// Resolve a reference in `view` across loose refs (including symbolic chains) and `packed-refs`.
pub(crate) fn lookup_ref_in_view(view: &View, full_ref: &[u8]) -> Result<ViewRefLookup> {
    let (packed, loose) = split_refs(&view.refs);
    let loose_bytes = loose.get(full_ref.as_bstr()).cloned();
    let packed_oid = packed
        .as_deref()
        .map(|bytes| find_packed_ref_oid(bytes.as_slice(), full_ref))
        .transpose()?
        .flatten();

    let resolved_oid = if loose_bytes.is_some() {
        let mut current = BString::from(full_ref);
        let mut found = None;
        for hop in 0..MAX_SYMREF_HOPS {
            if let Some(raw) = loose.get(&current) {
                let trimmed = trim_newline(raw.as_slice());
                if let Some(next) = trimmed.strip_prefix(b"ref: ") {
                    if hop + 1 == MAX_SYMREF_HOPS {
                        return Err(Error::UnsupportedRepository(
                            "symbolic reference exceeds maximum indirection depth".to_owned(),
                        ));
                    }
                    current = BString::from(next);
                    continue;
                }
                found = Some(GitOid::from_hex(trimmed)?);
                break;
            }
            found = packed
                .as_deref()
                .map(|bytes| find_packed_ref_oid(bytes.as_slice(), current.as_slice()))
                .transpose()?
                .flatten();
            break;
        }
        found
    } else {
        packed_oid
    };

    Ok(ViewRefLookup {
        loose_bytes,
        packed_oid,
        resolved_oid,
    })
}

/// Return the resolved symbolic ref path (`refs/...`) of `view.head`, or `None` when detached.
pub(crate) fn view_head_target_ref(view: &View) -> Result<Option<BString>> {
    let trimmed = trim_newline(view.head.as_slice());
    let Some(target) = trimmed.strip_prefix(b"ref: ") else {
        return Ok(None);
    };
    let (_, loose) = split_refs(&view.refs);
    let mut current = BString::from(target);
    for _ in 0..MAX_SYMREF_HOPS {
        if let Some(raw) = loose.get(&current)
            && let Some(next) = trim_newline(raw.as_slice()).strip_prefix(b"ref: ")
        {
            current = BString::from(next);
            continue;
        }
        return Ok(Some(current));
    }
    Err(Error::UnsupportedRepository(
        "symbolic HEAD exceeds maximum indirection depth".to_owned(),
    ))
}

/// Return the raw direct symbolic ref immediately stored in `.git/HEAD` (one hop, matching Git).
pub(crate) fn direct_head_symref(raw_head: &[u8]) -> Option<BString> {
    trim_newline(raw_head)
        .strip_prefix(b"ref: ")
        .map(BString::from)
}

/// List all loose symbolic refs in `view.refs` whose immediate target equals `target_ref`.
pub(crate) fn find_loose_symrefs_targeting(view: &View, target_ref: &[u8]) -> Vec<BString> {
    let mut callers = Vec::new();
    for (name, val) in &view.refs {
        if name.as_slice() == b"packed-refs" {
            continue;
        }
        if let Some(target) = trim_newline(val.as_slice()).strip_prefix(b"ref: ")
            && target == target_ref
        {
            callers.push(name.clone());
        }
    }
    callers
}

/// Read all refs under `prefix` (e.g. `b"refs/heads/"`) from `packed-refs` and loose files.
pub(crate) fn read_refs_with_prefix(
    meta: &CheckedRoot,
    prefix: &[u8],
) -> Result<BTreeMap<BString, GitOid>> {
    let mut out = BTreeMap::new();
    if let Some(packed) = meta.read_file_raw(b"packed-refs", MAX_CAS_BYTES as u64)? {
        for entry in
            gix::refs::packed::Iter::new(&packed, gix::hash::Kind::Sha1).map_err(|error| {
                Error::UnsupportedRepository(format!("invalid packed-refs: {error}"))
            })?
        {
            let reference = entry.map_err(|error| {
                Error::UnsupportedRepository(format!("corrupt packed-refs entry: {error}"))
            })?;
            let full_bytes = reference.name.as_bstr().as_bytes();
            if let Some(short) = full_bytes.strip_prefix(prefix) {
                let oid = GitOid::from_hex(reference.target.as_bytes())?;
                out.insert(BString::from(short), oid);
            }
        }
    }

    let loose_dir = prefix.strip_suffix(b"/").unwrap_or(prefix);
    let mut stack = vec![BString::from(loose_dir)];
    while let Some(dir) = stack.pop() {
        let entries = match meta.list_dir_entries(dir.as_slice()) {
            Ok(entries) => entries,
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for (name, kind) in entries {
            let mut rel = dir.clone();
            rel.push(b'/');
            rel.extend_from_slice(name.as_slice());
            match kind {
                PathEntryKind::Directory => stack.push(rel),
                PathEntryKind::Regular => {
                    if name.ends_with(b".lock") {
                        continue;
                    }
                    let (_, oid) = resolve_ref_chain(meta, rel.as_slice())?;
                    if let (Some(short), Some(oid)) = (rel.strip_prefix(prefix), oid) {
                        out.insert(BString::from(short), oid);
                    }
                }
                PathEntryKind::Executable | PathEntryKind::Symlink | PathEntryKind::Other => {
                    return Err(Error::UnsupportedRepository(format!(
                        "unsupported Git ref entry at {}",
                        escape_path_bytes(rel.as_slice())
                    )));
                }
            }
        }
    }
    Ok(out)
}

/// Collect every full ref name present as a loose ref or in `packed-refs` inside `view`.
pub(crate) fn collect_view_ref_names(view: &View) -> Result<BTreeSet<BString>> {
    let mut names = BTreeSet::new();
    for (name, val) in &view.refs {
        if name.as_slice() == b"packed-refs" {
            for entry in gix::refs::packed::Iter::new(val.as_slice(), gix::hash::Kind::Sha1)
                .map_err(|error| {
                    Error::UnsupportedRepository(format!("invalid packed-refs: {error}"))
                })?
            {
                let reference = entry.map_err(|error| {
                    Error::UnsupportedRepository(format!("corrupt packed-refs entry: {error}"))
                })?;
                names.insert(BString::from(reference.name.as_bstr().as_bytes()));
            }
        } else {
            names.insert(name.clone());
        }
    }
    Ok(names)
}

/// Refuse `candidate` if any existing ref (excluding `ignore`) collides via directory/file nesting.
pub(crate) fn check_ref_df_conflict(
    existing: &BTreeSet<BString>,
    candidate: &[u8],
    ignore: Option<&[u8]>,
) -> Result<()> {
    for ref_name in existing {
        let other = ref_name.as_slice();
        if ignore == Some(other) {
            continue;
        }
        let nested_under_other =
            candidate.starts_with(other) && candidate.get(other.len()).copied() == Some(b'/');
        let ancestor_of_other =
            other.starts_with(candidate) && other.get(candidate.len()).copied() == Some(b'/');
        if nested_under_other || ancestor_of_other {
            return Err(Error::UnsupportedRepository(format!(
                "reference {} conflicts with existing reference {}",
                escape_path_bytes(candidate),
                escape_path_bytes(other),
            )));
        }
    }
    Ok(())
}

/// Outcome of removing one entry from `packed-refs`.
pub(crate) enum PackedRefRemoval {
    NotFound,
    Emptied,
    Updated(BString),
}

/// Plan data returned by [`plan_ref_deletion`].
pub(crate) struct DeletedRefPlan {
    pub(crate) refs: Vec<(BString, BString)>,
    pub(crate) actions: Vec<crate::transaction::Action>,
    pub(crate) resolved_oid: GitOid,
}

/// Remove `target_ref` (and any immediately following peeled `^<oid>` line) from `packed-refs`.
pub(crate) fn remove_packed_ref(
    packed_bytes: &[u8],
    target_ref: &[u8],
) -> Result<PackedRefRemoval> {
    let mut out = Vec::with_capacity(packed_bytes.len());
    let mut found = false;
    let mut skipping_peeled = false;
    let mut remaining_entries = 0_usize;

    for raw_line in packed_bytes.split_inclusive(|&byte| byte == b'\n') {
        let trimmed = trim_newline(raw_line);
        if skipping_peeled && trimmed.starts_with(b"^") {
            continue;
        }
        skipping_peeled = false;

        if trimmed.is_empty() || trimmed.starts_with(b"#") || trimmed.starts_with(b"^") {
            out.extend_from_slice(raw_line);
            continue;
        }
        let Some(space) = trimmed.iter().position(|&byte| byte == b' ') else {
            return Err(Error::UnsupportedRepository(
                "malformed entry in packed-refs".to_owned(),
            ));
        };
        let oid_part = &trimmed[..space];
        GitOid::from_hex(oid_part)?;
        let ref_part = &trimmed[space + 1..];
        if ref_part == target_ref {
            found = true;
            skipping_peeled = true;
            continue;
        }
        remaining_entries += 1;
        out.extend_from_slice(raw_line);
    }

    if !found {
        return Ok(PackedRefRemoval::NotFound);
    }
    if remaining_entries == 0 {
        Ok(PackedRefRemoval::Emptied)
    } else {
        Ok(PackedRefRemoval::Updated(BString::from(out)))
    }
}

/// Remove `target_ref` from `packed_map` / loose `refs` inside a live `View`, returning rebuilt refs
/// and planned actions (`RemoveRef` and/or `WritePackedRefs`).
pub(crate) fn plan_ref_deletion(view: &View, full_ref: &[u8]) -> Result<DeletedRefPlan> {
    let lookup = lookup_ref_in_view(view, full_ref)?;
    let resolved_oid = lookup.resolved_oid.ok_or_else(|| {
        Error::UnsupportedRepository(format!(
            "reference {} does not exist",
            escape_path_bytes(full_ref)
        ))
    })?;

    let (mut packed, mut loose) = split_refs(&view.refs);
    let mut actions = Vec::new();
    if let Some(expected_before) = loose.remove(full_ref.as_bstr()) {
        actions.push(crate::transaction::Action::RemoveRef {
            name: BString::from(full_ref),
            expected_before,
        });
    }
    if let Some(current_packed) = packed.as_ref() {
        let updated_packed = match remove_packed_ref(current_packed.as_slice(), full_ref)? {
            PackedRefRemoval::NotFound => None,
            PackedRefRemoval::Emptied => Some(None),
            PackedRefRemoval::Updated(bytes) => Some(Some(bytes)),
        };
        if let Some(target_packed) = updated_packed {
            actions.push(crate::transaction::Action::WritePackedRefs {
                expected_before: Some(current_packed.clone()),
                target: target_packed.clone(),
            });
            packed = target_packed;
        }
    }

    Ok(DeletedRefPlan {
        refs: crate::reset::rebuild_refs(packed, loose),
        actions,
        resolved_oid,
    })
}

/// Follow annotated tag objects up to [`MAX_TAG_PEEL_HOPS`] times to reach a commit OID.
pub(crate) fn peel_to_commit(reader: &mut BoundedReader, mut oid: GitOid) -> Result<GitOid> {
    for _ in 0..MAX_TAG_PEEL_HOPS {
        let obj = reader.read(oid)?;
        match obj.kind {
            ObjectKind::Commit => return Ok(oid),
            ObjectKind::Tag => {
                let tag = Tag::parse(&obj.data)?;
                oid = tag.object;
            }
            other => {
                return Err(Error::UnsupportedRepository(format!(
                    "object {oid} is a {other}, expected commit"
                )));
            }
        }
    }
    Err(Error::UnsupportedRepository(
        "annotated tag chain exceeds maximum indirection depth".to_owned(),
    ))
}

/// Resolve `HEAD`, full 40-hex OIDs, local branches, or tags with optional `~N` parent steps.
pub(crate) fn resolve_revision_oid(
    meta: &CheckedRoot,
    reader: &mut BoundedReader,
    rev: &str,
    peel_commit: bool,
) -> Result<GitOid> {
    if rev.is_empty() || rev.contains('^') || rev.contains("@{") || rev.contains("..") {
        return Err(Error::UnsupportedRepository(format!(
            "unsupported revision syntax {rev:?}; use commit ID, HEAD, branch/tag name, and optional ~N"
        )));
    }
    let (base, steps) = parse_tilde_suffix(rev)?;
    let base_oid = resolve_base_oid(meta, reader, base)?;
    if !peel_commit && steps == 0 {
        return Ok(base_oid);
    }
    let commit_oid = peel_to_commit(reader, base_oid)?;
    walk_first_parent(reader, commit_oid, steps)
}

/// Return true when `ancestor` is reachable from `descendant` along any parent edge.
pub(crate) fn is_ancestor(
    reader: &mut BoundedReader,
    ancestor: GitOid,
    descendant: GitOid,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut visited = BTreeSet::new();
    let mut queue = VecDeque::new();
    visited.insert(descendant);
    queue.push_back(descendant);

    while let Some(oid) = queue.pop_front() {
        let obj = reader.read(oid)?;
        if obj.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "object {oid} is a {}, expected commit",
                obj.kind
            )));
        }
        let commit = Commit::parse(&obj.data)?;
        for parent in commit.parents {
            if parent == ancestor {
                return Ok(true);
            }
            if visited.len() >= MAX_ANCESTOR_VISIT {
                return Err(Error::UnsupportedRepository(format!(
                    "commit ancestry check exceeded {MAX_ANCESTOR_VISIT} commits"
                )));
            }
            if visited.insert(parent) {
                queue.push_back(parent);
            }
        }
    }
    Ok(false)
}

/// Read the first subject line of the commit named (or peeled) from `oid`.
pub(crate) fn commit_subject(reader: &mut BoundedReader, oid: GitOid) -> Result<BString> {
    let commit_oid = peel_to_commit(reader, oid)?;
    let obj = reader.read(commit_oid)?;
    let commit = Commit::parse(&obj.data)?;
    let msg = commit.message.as_slice();
    let line_end = msg
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(msg.len());
    Ok(BString::from(trim_newline(&msg[..line_end])))
}

fn find_packed_ref_oid(packed: &[u8], ref_name: &[u8]) -> Result<Option<GitOid>> {
    for entry in gix::refs::packed::Iter::new(packed, gix::hash::Kind::Sha1)
        .map_err(|error| Error::UnsupportedRepository(format!("invalid packed-refs: {error}")))?
    {
        let reference = entry.map_err(|error| {
            Error::UnsupportedRepository(format!("corrupt packed-refs entry: {error}"))
        })?;
        if reference.name.as_bstr().as_bytes() == ref_name {
            return Ok(Some(GitOid::from_hex(reference.target.as_bytes())?));
        }
    }
    Ok(None)
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
                    "unborn branch {}",
                    escape_path_bytes(leaf_ref.as_slice())
                ))
            });
        }
        return GitOid::from_hex(trimmed);
    }

    if base.len() == 40
        && let Ok(oid) = GitOid::from_hex(base.as_bytes())
    {
        reader.read(oid)?;
        return Ok(oid);
    }

    let branch_ref = format!("refs/heads/{base}");
    let tag_ref = format!("refs/tags/{base}");
    let branch_valid = gix::validate::reference::name(branch_ref.as_bytes().as_bstr()).is_ok();
    let tag_valid = gix::validate::reference::name(tag_ref.as_bytes().as_bstr()).is_ok();
    if !branch_valid && !tag_valid {
        return Err(Error::UnsupportedRepository(format!(
            "unsupported revision or reference name {base:?}"
        )));
    }
    let branch_oid = if branch_valid {
        resolve_ref_chain(meta, branch_ref.as_bytes())?.1
    } else {
        None
    };
    let tag_oid = if tag_valid {
        resolve_ref_chain(meta, tag_ref.as_bytes())?.1
    } else {
        None
    };

    match (branch_oid, tag_oid) {
        (Some(oid), None) | (None, Some(oid)) => Ok(oid),
        (Some(_), Some(_)) => Err(Error::UnsupportedRepository(format!(
            "ambiguous revision {base:?} matches both a branch and a tag"
        ))),
        (None, None) => Err(Error::UnsupportedRepository(format!(
            "unknown revision, branch, or tag {base:?}"
        ))),
    }
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
