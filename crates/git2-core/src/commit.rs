//! Native `git2 commit -m <message>` built on the durable pending journal.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::SystemTime;

use bstr::{BStr, BString, ByteSlice};
use gix::index::entry::{Flags, Mode};

use crate::object::{Commit, GitOid, ObjectKind, RawObject, Signature, Tree, TreeEntry, TreeMode};
use crate::paths::{CheckedRoot, checked_split};
use crate::recovery::{ContentId, RecoveryStore, View, check_view_size};
use crate::repository::{LocalRepository, known_cas_for_view, validate_index};
use crate::reset::{resolve_ref_chain, trim_newline};
use crate::restore::{decode_view_index_readonly, ensure_no_foreign_locks};
use crate::retention::{self, MAX_OBJECT_BYTES};
use crate::store::{BoundedReader, ObjectStore};
use crate::transaction::{self, Action, Failpoint, MutationPlan, escape_path_bytes};
use crate::{Error, Result};

const COMMIT_HOOKS: [&str; 4] = [
    "pre-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
];

/// Summary returned after a durable `git2 commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitOutcome {
    pub operation_id: ContentId,
    pub commit_oid: GitOid,
    pub tree_oid: GitOid,
    pub after_view: ContentId,
}

/// Record a new commit from the current staged index on `HEAD` or the current branch.
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, has a blank
/// commit message, has no staged changes relative to `HEAD`, activates commit hooks or signing,
/// lacks Git author/committer identity, has unsupported staged index entries (such as
/// intent-to-add), has a conflicting foreign lock, or fails durable execution.
pub fn apply(repo: &LocalRepository, message: &str) -> Result<CommitOutcome> {
    apply_with_hook(repo, message, |_| Ok(()))
}

#[doc(hidden)]
pub fn apply_with_hook(
    repo: &LocalRepository,
    message: &str,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<CommitOutcome> {
    let normalized_message = normalize_message(message)?;

    let store = RecoveryStore::open(repo.git_dir())?;
    let lock = store.try_lock()?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile or inspect it with `recover` first".to_owned(),
        ));
    }

    check_commit_config_and_hooks(repo)?;
    let now = gix::date::Time::now_local_or_utc();
    let author = resolve_signature(
        repo,
        "author",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_AUTHOR_DATE",
        now,
    )?;
    let committer = resolve_signature(
        repo,
        "committer",
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

    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let meta = CheckedRoot::open(repo.git_dir())?;
    let parent = resolve_parent_commit(&meta, &mut reader, live_view.head.as_slice())?;

    let mut generated = build_trees_from_index(live_index.as_ref())?;
    let root_tree_oid = generated.last().expect("root tree is always produced").0;
    ensure_commit_has_changes(parent, root_tree_oid, live_index.as_ref())?;

    let commit = Commit {
        tree: root_tree_oid,
        parents: parent.map(|(oid, _)| oid).into_iter().collect(),
        author,
        committer,
        extra_headers: Vec::new(),
        message: normalized_message,
    };
    let commit_raw = RawObject {
        kind: ObjectKind::Commit,
        data: commit.serialize(),
    };
    let commit_oid = commit_raw.id();
    generated.push((commit_oid, commit_raw));

    let head_plan = crate::reset::plan_head_update(&live_view, commit_oid)?;
    let known_cas = known_cas_for_view(&live_view);
    let (pending_objects, mut actions) =
        persist_pending_objects(&store, &mut reader, &known_cas, generated)?;
    actions.extend(head_plan.actions);
    ensure_no_foreign_locks(repo.git_dir(), &actions)?;

    let target_view_id = persist_commit_view(
        &store,
        &objects,
        &known_cas,
        &live_view,
        live_index.as_ref(),
        head_plan.head,
        head_plan.refs,
        &pending_objects,
    )?;

    let plan = MutationPlan {
        expected_head,
        before_view: live_view_id,
        target_view: target_view_id,
        command: BString::from("commit"),
        changes: MutationPlan::changes_for_actions(&actions),
        actions,
    };
    let operation_id = transaction::run_locked_with_hook(repo, &store, &lock, &plan, hook)?;
    Ok(CommitOutcome {
        operation_id,
        commit_oid,
        tree_oid: root_tree_oid,
        after_view: target_view_id,
    })
}

fn ensure_commit_has_changes(
    parent: Option<(GitOid, GitOid)>,
    root_tree_oid: GitOid,
    live_index: Option<&gix::index::State>,
) -> Result<()> {
    match parent {
        None => {
            if live_index.is_none_or(|state| state.entries().is_empty()) {
                return Err(Error::UnsupportedRepository(
                    "nothing to commit (initial commit requires at least one staged file)"
                        .to_owned(),
                ));
            }
        }
        Some((_, parent_tree)) if parent_tree == root_tree_oid => {
            return Err(Error::UnsupportedRepository(
                "nothing to commit; staged tree matches HEAD".to_owned(),
            ));
        }
        Some(_) => {}
    }
    Ok(())
}

type PendingObjectMap = BTreeMap<GitOid, (RawObject, ContentId)>;

fn persist_pending_objects(
    store: &RecoveryStore,
    reader: &mut BoundedReader,
    known_cas: &BTreeSet<ContentId>,
    generated: Vec<(GitOid, RawObject)>,
) -> Result<(PendingObjectMap, Vec<Action>)> {
    let mut pending_objects = BTreeMap::new();
    let mut actions = Vec::new();
    for (oid, raw) in generated {
        let content = retention::persist_cas_with_known(store, &raw.canonical_bytes(), known_cas)?;
        let already_present = match reader.read_physical(oid) {
            Ok(_) => true,
            Err(Error::ObjectNotFound(_)) => false,
            Err(error) => return Err(error),
        };
        if !already_present && !pending_objects.contains_key(&oid) {
            actions.push(Action::RehydrateObject { oid, content });
        }
        pending_objects.entry(oid).or_insert((raw, content));
    }
    Ok((pending_objects, actions))
}

fn normalize_message(message: &str) -> Result<BString> {
    if message.contains('\0') {
        return Err(Error::UnsupportedRepository(
            "commit message cannot contain NUL bytes".to_owned(),
        ));
    }
    if message.trim().is_empty() {
        return Err(Error::UnsupportedRepository(
            "aborting commit due to empty commit message".to_owned(),
        ));
    }
    let mut out = Vec::with_capacity(message.len() + 1);
    out.extend_from_slice(message.as_bytes());
    if !message.ends_with('\n') {
        out.push(b'\n');
    }
    Ok(BString::from(out))
}

fn check_commit_config_and_hooks(repo: &LocalRepository) -> Result<()> {
    let config = repo.inner.config_snapshot();
    if config.boolean("commit.gpgSign") == Some(true) {
        return Err(Error::UnsupportedRepository(
            "commit signing (`commit.gpgSign`) is not supported".to_owned(),
        ));
    }
    if let Some(encoding) = config.string("i18n.commitEncoding")
        && !encoding.eq_ignore_ascii_case(b"utf-8")
        && !encoding.eq_ignore_ascii_case(b"utf8")
    {
        return Err(Error::UnsupportedRepository(format!(
            "unsupported `i18n.commitEncoding` value {:?}",
            encoding.to_str_lossy()
        )));
    }

    let hooks_dir = resolve_hooks_dir(repo, &config)?;
    let Some(hooks_dir) = hooks_dir else {
        return Ok(());
    };
    for hook in COMMIT_HOOKS {
        let path = hooks_dir.join(hook);
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() && (meta.permissions().mode() & 0o111) == 0 => {}
            Ok(_) => {
                return Err(Error::UnsupportedRepository(format!(
                    "active Git hook `{hook}` at {} is not supported",
                    path.display()
                )));
            }
            Err(error) => {
                return Err(Error::UnsupportedRepository(format!(
                    "Git hook `{hook}` at {} blocks commit ({error})",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn resolve_hooks_dir(
    repo: &LocalRepository,
    config: &gix::config::Snapshot<'_>,
) -> Result<Option<PathBuf>> {
    if let Some(raw) = config.string("core.hooksPath")
        && raw.is_empty()
    {
        return Ok(None);
    }
    if let Some(configured) = config.trusted_path("core.hooksPath") {
        let path = configured.map_err(|error| Error::Gix(error.to_string()))?;
        let resolved = if path.is_absolute() {
            path.into_owned()
        } else {
            repo.workdir().join(path.as_ref())
        };
        return Ok(Some(resolved));
    }
    Ok(Some(repo.git_dir().join("hooks")))
}

fn resolve_signature(
    repo: &LocalRepository,
    role: &str,
    name_env: &str,
    email_env: &str,
    date_env: &str,
    default_time: gix::date::Time,
) -> Result<Signature> {
    let config = repo.inner.config_snapshot();
    let name = resolve_identity_part(
        role,
        "name",
        name_env,
        config.string("user.name").as_deref(),
    )?;
    let email = resolve_identity_part(
        role,
        "email",
        email_env,
        config.string("user.email").as_deref(),
    )?;
    let (timestamp, timezone_offset_minutes) = resolve_signature_time(date_env, default_time)?;
    Ok(Signature {
        name,
        email,
        timestamp,
        timezone_offset_minutes,
    })
}

fn resolve_identity_part(
    role: &str,
    field: &str,
    env_name: &str,
    config_val: Option<&BStr>,
) -> Result<BString> {
    let raw = if let Some(env) = std::env::var_os(env_name)
        && !env.is_empty()
    {
        BString::from(env.as_bytes())
    } else if let Some(cfg) = config_val {
        BString::from(cfg.as_bytes())
    } else {
        return Err(Error::UnsupportedRepository(format!(
            "{role} {field} is not configured; set Git `user.{field}` or `{env_name}`"
        )));
    };
    let trimmed = trim_ascii_whitespace(raw.as_slice());
    if trimmed.is_empty() {
        return Err(Error::UnsupportedRepository(format!(
            "{role} {field} cannot be blank"
        )));
    }
    if trimmed
        .iter()
        .any(|&byte| matches!(byte, b'<' | b'>' | b'\n' | b'\r' | 0))
    {
        return Err(Error::UnsupportedRepository(format!(
            "{role} {field} contains forbidden header characters"
        )));
    }
    Ok(BString::from(trimmed))
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn resolve_signature_time(env_name: &str, default_time: gix::date::Time) -> Result<(i64, i16)> {
    let time = if let Some(raw) = std::env::var_os(env_name)
        && !raw.is_empty()
    {
        let text = raw.to_str().ok_or_else(|| {
            Error::UnsupportedRepository(format!("{env_name} must be valid UTF-8"))
        })?;
        gix::date::parse(text, Some(SystemTime::now())).map_err(|error| {
            Error::UnsupportedRepository(format!("invalid {env_name} value {text:?}: {error}"))
        })?
    } else {
        default_time
    };
    let offset_minutes = offset_seconds_to_minutes(time.offset)?;
    Ok((time.seconds, offset_minutes))
}

fn offset_seconds_to_minutes(offset_seconds: i32) -> Result<i16> {
    if offset_seconds % 60 != 0 {
        return Err(Error::UnsupportedRepository(format!(
            "timezone offset {offset_seconds}s is not a whole number of minutes"
        )));
    }
    let minutes = i16::try_from(offset_seconds / 60)
        .map_err(|_| Error::UnsupportedRepository("timezone offset out of range".to_owned()))?;
    if minutes.abs() > 23 * 60 + 59 {
        return Err(Error::UnsupportedRepository(format!(
            "timezone offset {minutes}m is out of range"
        )));
    }
    Ok(minutes)
}

fn resolve_parent_commit(
    meta: &CheckedRoot,
    reader: &mut BoundedReader,
    raw_head: &[u8],
) -> Result<Option<(GitOid, GitOid)>> {
    let trimmed = trim_newline(raw_head);
    let parent_oid = if let Some(target_ref) = trimmed.strip_prefix(b"ref: ") {
        let (_, oid) = resolve_ref_chain(meta, target_ref)?;
        let Some(oid) = oid else {
            return Ok(None);
        };
        oid
    } else {
        GitOid::from_hex(trimmed)?
    };
    let object = reader.read(parent_oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "HEAD object {parent_oid} is {}, expected commit",
            object.kind
        )));
    }
    let parent_commit = Commit::parse(&object.data)?;
    Ok(Some((parent_oid, parent_commit.tree)))
}

#[derive(Default)]
struct TreeNode {
    files: BTreeMap<BString, (TreeMode, GitOid)>,
    dirs: BTreeMap<BString, TreeNode>,
}

impl TreeNode {
    fn insert(
        &mut self,
        dirs: &[&[u8]],
        leaf: &[u8],
        mode: TreeMode,
        oid: GitOid,
        full_path: &[u8],
    ) -> Result<()> {
        if let Some((head, tail)) = dirs.split_first() {
            let name = BString::from(*head);
            if self.files.contains_key(&name) {
                return Err(Error::UnsupportedRepository(format!(
                    "file/directory conflict in index at {}",
                    escape_path_bytes(full_path)
                )));
            }
            return self
                .dirs
                .entry(name)
                .or_default()
                .insert(tail, leaf, mode, oid, full_path);
        }
        let name = BString::from(leaf);
        if self.dirs.contains_key(&name) || self.files.contains_key(&name) {
            return Err(Error::UnsupportedRepository(format!(
                "conflicting index path {}",
                escape_path_bytes(full_path)
            )));
        }
        self.files.insert(name, (mode, oid));
        Ok(())
    }

    fn serialize_bottom_up(self, out: &mut Vec<(GitOid, RawObject)>) -> GitOid {
        let mut entries = Vec::with_capacity(self.files.len() + self.dirs.len());
        for (name, (mode, oid)) in self.files {
            entries.push(TreeEntry { mode, name, oid });
        }
        for (name, child) in self.dirs {
            let child_oid = child.serialize_bottom_up(out);
            entries.push(TreeEntry {
                mode: TreeMode::Directory,
                name,
                oid: child_oid,
            });
        }
        entries.sort_by(cmp_tree_entries);
        let raw = RawObject {
            kind: ObjectKind::Tree,
            data: Tree { entries }.serialize(),
        };
        let oid = raw.id();
        out.push((oid, raw));
        oid
    }
}

fn cmp_tree_entries(left: &TreeEntry, right: &TreeEntry) -> std::cmp::Ordering {
    let l = left.name.as_slice();
    let r = right.name.as_slice();
    let common = l.len().min(r.len());
    match l[..common].cmp(&r[..common]) {
        std::cmp::Ordering::Equal => {
            let l_ext = if l.len() > common {
                Some(l[common])
            } else if left.mode == TreeMode::Directory {
                Some(b'/')
            } else {
                None
            };
            let r_ext = if r.len() > common {
                Some(r[common])
            } else if right.mode == TreeMode::Directory {
                Some(b'/')
            } else {
                None
            };
            match (l_ext, r_ext) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (Some(a), Some(b)) => a.cmp(&b),
            }
        }
        ordering => ordering,
    }
}

fn build_trees_from_index(index: Option<&gix::index::State>) -> Result<Vec<(GitOid, RawObject)>> {
    let mut root = TreeNode::default();
    if let Some(index) = index {
        validate_index(index)?;
        if index.resolve_undo().is_some() {
            return Err(Error::UnsupportedRepository(
                "indexes with resolve-undo (REUC) records are not supported".to_owned(),
            ));
        }
        for entry in index.entries() {
            let path = entry.path(index);
            if entry.flags.contains(Flags::INTENT_TO_ADD) {
                return Err(Error::UnsupportedRepository(format!(
                    "cannot commit intent-to-add entry {}; stage or unstage it first",
                    escape_path_bytes(path.as_bytes())
                )));
            }
            let mode = match entry.mode {
                Mode::FILE => TreeMode::File,
                Mode::FILE_EXECUTABLE => TreeMode::Executable,
                Mode::SYMLINK => TreeMode::Symlink,
                _ => {
                    return Err(Error::UnsupportedRepository(format!(
                        "unsupported staged entry mode {:?} for {}",
                        entry.mode,
                        escape_path_bytes(path.as_bytes())
                    )));
                }
            };
            let oid_bytes: [u8; 20] = entry.id.as_slice().try_into().map_err(|_| {
                Error::CorruptObject("index entry object id has unexpected length".to_owned())
            })?;
            let (dirs, leaf) = checked_split(path.as_bytes())?;
            root.insert(
                &dirs,
                leaf,
                mode,
                GitOid::from_bytes(oid_bytes),
                path.as_bytes(),
            )?;
        }
    }
    let mut generated = Vec::new();
    root.serialize_bottom_up(&mut generated);
    Ok(generated)
}

#[allow(clippy::too_many_arguments)]
fn persist_commit_view(
    store: &RecoveryStore,
    objects: &ObjectStore,
    known_cas: &BTreeSet<ContentId>,
    live_view: &View,
    live_index: Option<&gix::index::State>,
    head: BString,
    refs: Vec<(BString, BString)>,
    pending_objects: &BTreeMap<GitOid, (RawObject, ContentId)>,
) -> Result<ContentId> {
    let roots = retention::roots(head.as_slice(), &refs, live_index)?;
    check_view_size(
        head.len(),
        &refs,
        live_view.index.is_some(),
        live_view.files.iter().map(|file| file.path.len()),
        roots.len(),
    )?;
    let retained_objects =
        retention::retain_roots_with_objects(store, objects, roots, pending_objects, known_cas)?;
    check_view_size(
        head.len(),
        &refs,
        live_view.index.is_some(),
        live_view.files.iter().map(|file| file.path.len()),
        retained_objects.len(),
    )?;
    let view = View {
        head,
        refs,
        index: live_view.index,
        files: live_view.files.clone(),
        retained_objects,
    };
    store.put_view(&view)
}
