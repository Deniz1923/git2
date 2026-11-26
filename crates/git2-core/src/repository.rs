//! Repository validation and first-time recovery adoption.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use bstr::{BStr, BString, ByteSlice};
use sha1::{Digest, Sha1};

use crate::paths::CheckedRoot;
use crate::recovery::{
    CapturePolicy, ContentId, FileKind, FileRecord, MAX_CAS_BYTES, MAX_FIELD_BYTES, Operation,
    OperationKind, RecoveryStore, View, check_view_size,
};
use crate::retention::{self, Root};
use crate::store::ObjectStore;
use crate::{Error, Result};

/// Safety limits persisted when adopting an existing repository.
pub type AdoptOptions = CapturePolicy;

/// Summary of a newly persisted baseline view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adoption {
    pub operation_id: ContentId,
    pub view_id: ContentId,
    pub file_count: usize,
    pub content_bytes: u64,
}

/// Result of checking an adopted repository for changes made by ordinary Git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalImport {
    pub changed: bool,
    pub operation_id: ContentId,
    pub view_id: ContentId,
    pub file_count: usize,
    pub content_bytes: u64,
}

/// A validated local Git repository.
pub struct LocalRepository {
    pub(crate) inner: gix::Repository,
    git_dir: PathBuf,
    workdir: PathBuf,
}

impl LocalRepository {
    /// Discover and validate a supported repository.
    ///
    /// # Errors
    ///
    /// Returns an error if discovery fails or the repository uses a feature the recovery engine
    /// cannot yet restore safely.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let inner = gix::discover(path.as_ref()).map_err(|error| Error::Gix(error.to_string()))?;
        Self::validate(inner)
    }

    fn validate(inner: gix::Repository) -> Result<Self> {
        if inner.object_hash() != gix::hash::Kind::Sha1 {
            return Err(Error::UnsupportedRepository(
                "only SHA-1 repositories are supported".to_owned(),
            ));
        }
        let workdir = inner.workdir().ok_or_else(|| {
            Error::UnsupportedRepository("bare repositories are not supported".to_owned())
        })?;
        let workdir = workdir.to_path_buf();
        if inner.common_dir() != inner.git_dir() {
            return Err(Error::UnsupportedRepository(
                "linked worktrees are not supported".to_owned(),
            ));
        }
        if !fs::symlink_metadata(workdir.join(".git"))
            .is_ok_and(|metadata| metadata.file_type().is_dir())
        {
            return Err(Error::UnsupportedRepository(
                "Git metadata must live in the worktree's own `.git` directory".to_owned(),
            ));
        }

        let git_dir = inner.git_dir().to_path_buf();
        for state in [
            "BISECT_LOG",
            "CHERRY_PICK_HEAD",
            "MERGE_HEAD",
            "REVERT_HEAD",
            "rebase-apply",
            "rebase-merge",
            "sequencer",
        ] {
            if git_dir.join(state).exists() {
                return Err(Error::UnsupportedRepository(format!(
                    "an in-progress Git operation ({state}) must be completed or aborted first"
                )));
            }
        }
        if git_dir.join("worktrees").exists() {
            return Err(Error::UnsupportedRepository(
                "repositories with linked worktrees are not supported".to_owned(),
            ));
        }
        if git_dir.join("reftable").exists() {
            return Err(Error::UnsupportedRepository(
                "only file-backed references are supported".to_owned(),
            ));
        }
        if git_dir
            .join("objects")
            .join("info")
            .join("alternates")
            .exists()
        {
            return Err(Error::UnsupportedRepository(
                "alternate object databases are not supported".to_owned(),
            ));
        }
        if git_dir.join("shallow").exists() {
            return Err(Error::UnsupportedRepository(
                "shallow clones are not supported".to_owned(),
            ));
        }
        let config = inner.config_snapshot();
        if config.boolean("core.sparseCheckout") == Some(true) {
            return Err(Error::UnsupportedRepository(
                "sparse indexes and sparse checkouts are not supported".to_owned(),
            ));
        }
        if has_promisor_pack(&git_dir)?
            || has_promisor_remote(&config, &inner)
            || config.string("extensions.partialClone").is_some()
        {
            return Err(Error::UnsupportedRepository(
                "partial clones are not supported".to_owned(),
            ));
        }

        let metadata = CheckedRoot::open(&git_dir)?;
        if let Some(index_bytes) = read_metadata(&metadata, b"index", MAX_CAS_BYTES)? {
            let index = decode_index(&index_bytes)?;
            validate_index(&index)?;
        }

        Ok(Self {
            inner,
            git_dir,
            workdir,
        })
    }

    /// Return the Git metadata directory.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Return the root of the working tree.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Capture the repository's complete supported local state and establish its recovery store.
    ///
    /// # Errors
    ///
    /// Returns an error before creating the recovery store if any eligible file is unsupported or
    /// exceeds the configured per-file limit. I/O or concurrent repository changes can also fail
    /// the capture.
    pub fn adopt(&self, options: AdoptOptions) -> Result<Adoption> {
        if RecoveryStore::exists(&self.git_dir)? {
            // Surface failures such as legacy format or unsafe permissions, rather
            // than asking the user to start over without knowing what exists.
            RecoveryStore::open(&self.git_dir)?.head()?;
            return Err(Error::UnsupportedRepository(
                "this repository already has a git2 recovery store".to_owned(),
            ));
        }
        let options = options.validate()?;
        let draft = self.capture_draft(options)?;
        let store = RecoveryStore::initialize(&self.git_dir, options)?;
        let _lock = store.try_lock()?;
        let persisted = self.persist_draft(&store, draft)?;
        let operation = Operation {
            parent: None,
            before: persisted.view_id,
            after: persisted.view_id,
            kind: OperationKind::Adopt,
            timestamp_seconds: timestamp_seconds(),
        };
        let operation_id = store.put_operation(&operation)?;
        store.set_head(operation_id)?;
        Ok(Adoption {
            operation_id,
            view_id: persisted.view_id,
            file_count: persisted.file_count,
            content_bytes: persisted.content_bytes,
        })
    }

    /// Import a new immutable view if ordinary Git changed an adopted repository.
    ///
    /// # Errors
    ///
    /// Returns an error if the repository has not been adopted, another writer holds the recovery
    /// lock, or the new state cannot be captured and persisted safely.
    pub fn import_external_changes(&self) -> Result<ExternalImport> {
        self.record_view(OperationKind::ExternalGit)
    }

    /// Record an explicit checkpoint in an adopted repository.
    ///
    /// # Errors
    ///
    /// Returns an error if a complete object graph cannot be captured, the store is
    /// unavailable, or another git2 process holds the lock.
    pub fn checkpoint(&self) -> Result<ExternalImport> {
        if !RecoveryStore::exists(&self.git_dir)? {
            return Err(Error::UnsupportedRepository(
                "run `git2 adopt` before saving a checkpoint".to_owned(),
            ));
        }
        self.record_view(OperationKind::Git2(BString::from("checkpoint")))
    }

    fn record_view(&self, kind: OperationKind) -> Result<ExternalImport> {
        let store = RecoveryStore::open(&self.git_dir)?;
        let _lock = store.try_lock()?;
        let parent = store.head()?;
        let before = store.current_view()?;
        let draft = self.capture_draft(store.policy()?)?;
        let persisted = self.persist_draft(&store, draft)?;
        if persisted.view_id == before {
            return Ok(ExternalImport {
                changed: false,
                operation_id: parent,
                view_id: before,
                file_count: persisted.file_count,
                content_bytes: persisted.content_bytes,
            });
        }
        let operation = Operation {
            parent: Some(parent),
            before,
            after: persisted.view_id,
            kind,
            timestamp_seconds: timestamp_seconds(),
        };
        let operation_id = store.put_operation(&operation)?;
        store.set_head(operation_id)?;
        Ok(ExternalImport {
            changed: true,
            operation_id,
            view_id: persisted.view_id,
            file_count: persisted.file_count,
            content_bytes: persisted.content_bytes,
        })
    }

    fn capture_draft(&self, options: AdoptOptions) -> Result<ViewDraft> {
        let metadata = CheckedRoot::open(&self.git_dir)?;
        let head = read_metadata(&metadata, b"HEAD", MAX_FIELD_BYTES)?
            .ok_or_else(|| Error::UnsupportedRepository("Git HEAD is missing".to_owned()))?;
        let refs = capture_refs(&self.git_dir, &metadata)?;
        let index_bytes = read_metadata(&metadata, b"index", MAX_CAS_BYTES)?;
        let index = index_bytes.as_deref().map(decode_index).transpose()?;
        // Validate the same semantics that discovery checked, so the raw bytes about to
        // be persisted can only carry modeled index states even if Git changed them
        // after discovery.
        let roots = retention::roots(&head, &refs, index.as_ref())?;

        let mut paths = BTreeSet::<Vec<u8>>::new();
        if let Some(index) = &index {
            for entry in index.entries() {
                paths.insert(entry.path(index).to_vec());
            }
        }
        for path in self.untracked_paths(index.as_ref())? {
            paths.insert(path);
        }

        let root = CheckedRoot::open(&self.workdir)?;
        let mut files = Vec::with_capacity(paths.len());
        for path in &paths {
            // Kinds and bytes come from the worktree itself: an executable bit cleared on
            // disk must be captured as regular, and a leaf symlink as its raw target.
            let Some(entry) = root.read_entry(path, options.max_file_bytes)? else {
                continue;
            };
            files.push(FileDraft {
                path: BString::from(path.as_slice()),
                kind: entry.kind,
                bytes: entry.bytes,
            });
        }

        check_view_size(
            head.len(),
            &refs,
            index_bytes.is_some(),
            files.iter().map(|file| file.path.len()),
            0,
        )?;
        Ok(ViewDraft {
            head: head.into(),
            refs,
            index: index_bytes,
            files,
            roots,
        })
    }

    fn persist_draft(&self, store: &RecoveryStore, draft: ViewDraft) -> Result<PersistedDraft> {
        let objects = ObjectStore::from_repository(self.inner.clone())?;
        let retained = retention::retain_roots(store, &objects, draft.roots.iter().copied())?;
        check_view_size(
            draft.head.len(),
            &draft.refs,
            draft.index.is_some(),
            draft.files.iter().map(|file| file.path.len()),
            retained.len(),
        )?;
        persist_draft(store, draft, retained)
    }

    /// Enumerate non-ignored untracked worktree paths with a checked directory walk.
    ///
    /// The walk applies repository ignore rules but never runs content conversion or
    /// configured executable filters. Nested repositories and special entries are
    /// ineligible rather than silently skipped.
    ///
    /// # Errors
    ///
    /// Returns an error if the walk fails or an eligible untracked entry is a nested
    /// repository or a special entry such as a FIFO, socket, or device.
    fn untracked_paths(&self, index: Option<&gix::index::State>) -> Result<Vec<Vec<u8>>> {
        let empty_index = gix::index::State::new(self.inner.object_hash());
        let index = index.unwrap_or(&empty_index);
        let should_interrupt = AtomicBool::new(false);
        let mut collector = UntrackedDelegate::default();
        let walk_options = self
            .inner
            .dirwalk_options()
            .map_err(|error| Error::Gix(error.to_string()))?
            .empty_patterns_match_prefix(false)
            .emit_untracked(gix::dir::walk::EmissionMode::Matching)
            .classify_untracked_bare_repositories(true);
        self.inner
            .dirwalk(
                index,
                std::iter::empty::<&BStr>(),
                &should_interrupt,
                walk_options,
                &mut collector,
            )
            .map_err(|error| Error::Gix(error.to_string()))?;
        if let Some((path, kind)) = collector.refused {
            return Err(Error::UnsupportedRepository(format!(
                "untracked entry {path:?} cannot be captured faithfully (disk kind {kind:?})"
            )));
        }
        Ok(collector.untracked)
    }
}

/// Collects eligible untracked entries and remembers the first unsupported one.
#[derive(Default)]
struct UntrackedDelegate {
    untracked: Vec<Vec<u8>>,
    refused: Option<(BString, Option<gix::dir::entry::Kind>)>,
}

impl gix::dir::walk::Delegate for UntrackedDelegate {
    fn emit(
        &mut self,
        entry: gix::dir::EntryRef<'_>,
        _collapsed_directory_status: Option<gix::dir::entry::Status>,
    ) -> gix::dir::walk::Action {
        if !matches!(entry.status, gix::dir::entry::Status::Untracked) {
            return gix::dir::walk::Action::Continue(());
        }
        if !matches!(
            entry.disk_kind,
            Some(gix::dir::entry::Kind::File | gix::dir::entry::Kind::Symlink)
        ) {
            // An eligible untracked entry that is not a plain file or symlink - a
            // nested repository, a special node, or a kind the walk left undetermined -
            // cannot be captured faithfully.
            self.refused = Some((entry.rela_path.clone().into_owned(), entry.disk_kind));
            return gix::dir::walk::Action::Break(());
        }
        self.untracked.push(entry.rela_path.to_vec());
        gix::dir::walk::Action::Continue(())
    }
}

struct ViewDraft {
    head: BString,
    refs: Vec<(BString, BString)>,
    index: Option<Vec<u8>>,
    files: Vec<FileDraft>,
    roots: Vec<Root>,
}

struct FileDraft {
    path: BString,
    kind: FileKind,
    bytes: Vec<u8>,
}

struct PersistedDraft {
    view_id: ContentId,
    file_count: usize,
    content_bytes: u64,
}

fn persist_draft(
    store: &RecoveryStore,
    draft: ViewDraft,
    retained_objects: Vec<crate::recovery::RetainedObject>,
) -> Result<PersistedDraft> {
    let index = draft
        .index
        .as_deref()
        .map(|bytes| store.put_content(bytes))
        .transpose()?;
    let mut files = Vec::with_capacity(draft.files.len());
    let mut content_bytes = 0_u64;
    for file in draft.files {
        content_bytes = content_bytes.saturating_add(file.bytes.len() as u64);
        files.push(FileRecord {
            path: file.path,
            kind: file.kind,
            content: store.put_content(&file.bytes)?,
        });
    }
    let file_count = files.len();
    let view = View {
        head: draft.head,
        refs: draft.refs,
        index,
        files,
        retained_objects,
    };
    Ok(PersistedDraft {
        view_id: store.put_view(&view)?,
        file_count,
        content_bytes,
    })
}

fn timestamp_seconds() -> i64 {
    let now = std::time::SystemTime::now();
    match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        Err(error) => -i64::try_from(error.duration().as_secs()).unwrap_or(i64::MAX),
    }
}

fn has_promisor_pack(git_dir: &Path) -> Result<bool> {
    let pack_dir = git_dir.join("objects").join("pack");
    let entries = match fs::read_dir(pack_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        if entry?
            .path()
            .extension()
            .is_some_and(|extension| extension == "promisor")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn read_metadata(root: &CheckedRoot, path: &[u8], limit: usize) -> Result<Option<Vec<u8>>> {
    let Some(entry) = root.read_entry(path, limit as u64)? else {
        return Ok(None);
    };
    if entry.kind == FileKind::Symlink {
        return Err(Error::UnsupportedRepository(format!(
            "Git metadata {:?} must not be a symlink",
            BString::from(path)
        )));
    }
    Ok(Some(entry.bytes))
}

fn capture_refs(git_dir: &Path, root: &CheckedRoot) -> Result<Vec<(BString, BString)>> {
    let mut refs = Vec::new();
    let refs_root = git_dir.join("refs");
    match fs::symlink_metadata(&refs_root) {
        Ok(metadata) if metadata.is_dir() => {
            capture_ref_directory(git_dir, &refs_root, root, &mut refs)?;
        }
        Ok(_) => {
            return Err(Error::UnsupportedRepository(
                "Git metadata refs must be a real directory".to_owned(),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(packed) = read_metadata(root, b"packed-refs", MAX_FIELD_BYTES)? {
        refs.push((
            BString::from(b"packed-refs".to_vec()),
            BString::from(packed),
        ));
    }
    refs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(refs)
}

fn capture_ref_directory(
    git_dir: &Path,
    directory: &Path,
    root: &CheckedRoot,
    output: &mut Vec<(BString, BString)>,
) -> Result<()> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                pending.push(path);
                continue;
            }
            if path.extension().is_some_and(|value| value == "lock") {
                continue;
            }
            let relative = path
                .strip_prefix(git_dir)
                .map_err(|_| Error::CorruptRecovery("reference escaped Git directory".to_owned()))?
                .as_os_str()
                .as_bytes();
            let value = read_metadata(root, relative, MAX_FIELD_BYTES)?.ok_or_else(|| {
                Error::UnsupportedRepository(format!(
                    "Git ref {:?} disappeared during capture",
                    BString::from(relative)
                ))
            })?;
            output.push((BString::from(relative), BString::from(value)));
        }
    }
    Ok(())
}

/// Decode the raw index once, with its checksum verified and allocations bounded.
///
/// Decoding from raw bytes keeps the split-index link extension observable, which
/// [`gix::index::File::at`] dissolves silently, and bounds every decode allocation by the
/// index's own size.
///
/// # Errors
///
/// Returns an error if the index bytes are shorter than a checksum, fail checksum
/// verification, or cannot be decoded.
pub(crate) fn decode_index(bytes: &[u8]) -> Result<gix::index::State> {
    let checksum_bytes = gix::hash::Kind::Sha1.len_in_bytes();
    if bytes.len() < checksum_bytes {
        return Err(Error::Gix(
            "the index is shorter than its checksum".to_owned(),
        ));
    }
    let (data, expected) = bytes.split_at(bytes.len() - checksum_bytes);
    if !expected.iter().all(|byte| *byte == 0) {
        let digest = Sha1::digest(data);
        if digest.as_slice() != expected {
            return Err(Error::Gix("index checksum mismatch".to_owned()));
        }
    }
    let options = gix::index::decode::Options {
        thread_limit: Some(1),
        min_extension_block_in_bytes_for_threading: usize::MAX,
        expected_checksum: None,
        alloc_limit_bytes: Some(bytes.len().saturating_mul(4)),
    };
    let (index, _) = gix::index::State::from_bytes(
        bytes,
        SystemTime::UNIX_EPOCH.into(),
        gix::hash::Kind::Sha1,
        options,
    )
    .map_err(|error| Error::Gix(error.to_string()))?;
    Ok(index)
}

/// Refuse index semantics that recovery cannot restore faithfully.
///
/// The same validation runs at discovery and again at capture, so the raw index bytes that
/// are about to be persisted can only carry modeled semantics even if ordinary Git
/// changed them in between.
///
/// # Errors
///
/// Returns an error if the index is split, sparse by marker or skip-worktree flag, has
/// unresolved conflict stages, or contains gitlink modes.
pub(crate) fn validate_index(index: &gix::index::State) -> Result<()> {
    if index.link().is_some() {
        return Err(Error::UnsupportedRepository(
            "split indexes are not supported".to_owned(),
        ));
    }
    if index.is_sparse() {
        return Err(Error::UnsupportedRepository(
            "sparse indexes and sparse checkouts are not supported".to_owned(),
        ));
    }
    for entry in index.entries() {
        if entry
            .flags
            .contains(gix::index::entry::Flags::SKIP_WORKTREE)
        {
            return Err(Error::UnsupportedRepository(
                "sparse indexes and sparse checkouts are not supported".to_owned(),
            ));
        }
        if entry.stage_raw() != 0 {
            return Err(Error::UnsupportedRepository(
                "an index with unresolved conflicts is not supported".to_owned(),
            ));
        }
        if entry.mode == gix::index::entry::Mode::COMMIT {
            return Err(Error::UnsupportedRepository(
                "submodules are not supported".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Return true if any configured remote is marked as a promisor.
fn has_promisor_remote(config: &gix::config::Snapshot<'_>, inner: &gix::Repository) -> bool {
    inner.remote_names().into_iter().any(|name| {
        let mut key = BString::from(b"remote.".to_vec());
        key.extend_from_slice(name.as_bytes());
        key.extend_from_slice(b".promisor");
        config.boolean(key.as_bstr()) == Some(true)
    })
}
