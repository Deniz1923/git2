//! Repository validation and first-time recovery adoption.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use bstr::BString;

use crate::recovery::{
    ContentId, FileKind, FileRecord, Operation, OperationKind, RecoveryStore, View,
};
use crate::{Error, Result};

/// Default maximum size of one eligible worktree file: 100 MiB.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Safety limits used while adopting an existing repository.
#[derive(Debug, Clone, Copy)]
pub struct AdoptOptions {
    pub max_file_bytes: u64,
}

impl Default for AdoptOptions {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

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
        if has_promisor_pack(&git_dir)? {
            return Err(Error::UnsupportedRepository(
                "partial clones are not supported".to_owned(),
            ));
        }

        let index = inner
            .index_or_empty()
            .map_err(|error| Error::Gix(error.to_string()))?;
        if index.is_sparse() {
            return Err(Error::UnsupportedRepository(
                "sparse indexes and sparse checkouts are not supported".to_owned(),
            ));
        }
        for entry in index.entries() {
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
        if self.git_dir.join("git2").exists() {
            return Err(Error::UnsupportedRepository(
                "this repository already has a git2 recovery store".to_owned(),
            ));
        }
        let draft = self.capture_draft(options)?;
        let store = RecoveryStore::initialize(&self.git_dir)?;
        let _lock = store.try_lock()?;
        let persisted = persist_draft(&store, draft)?;
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
    pub fn import_external_changes(&self, options: AdoptOptions) -> Result<ExternalImport> {
        let store = RecoveryStore::open(&self.git_dir)?;
        let _lock = store.try_lock()?;
        let parent = store.head()?;
        let before = store.current_view()?;
        let draft = self.capture_draft(options)?;
        let persisted = persist_draft(&store, draft)?;
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
            kind: OperationKind::ExternalGit,
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
        let head = BString::from(fs::read(self.git_dir.join("HEAD"))?);
        let refs = capture_refs(&self.git_dir)?;
        let index_path = self.inner.index_path();
        let index = index_path
            .is_file()
            .then(|| fs::read(index_path))
            .transpose()?;

        let parsed_index = self
            .inner
            .index_or_empty()
            .map_err(|error| Error::Gix(error.to_string()))?;
        let mut paths = BTreeMap::<Vec<u8>, FileKind>::new();
        for entry in parsed_index.entries() {
            let kind = match entry.mode {
                mode if mode == gix::index::entry::Mode::FILE => FileKind::Regular,
                mode if mode == gix::index::entry::Mode::FILE_EXECUTABLE => FileKind::Executable,
                mode if mode == gix::index::entry::Mode::SYMLINK => FileKind::Symlink,
                _ => {
                    return Err(Error::UnsupportedRepository(format!(
                        "unsupported index mode {:?}",
                        entry.mode
                    )));
                }
            };
            paths.insert(entry.path(&parsed_index).to_vec(), kind);
        }

        let statuses = self
            .inner
            .status(gix::progress::Discard)
            .map_err(|error| Error::Gix(error.to_string()))?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_submodules(None)
            .into_index_worktree_iter(Vec::<BString>::new())
            .map_err(|error| Error::Gix(error.to_string()))?;
        for status in statuses {
            let status = status.map_err(|error| Error::Gix(error.to_string()))?;
            if status.summary() == Some(gix::status::index_worktree::iter::Summary::Added) {
                let path = status.rela_path().to_vec();
                let absolute = worktree_path(&self.workdir, &path)?;
                let metadata = fs::symlink_metadata(&absolute)?;
                let kind = kind_from_metadata(&metadata)?;
                paths.entry(path).or_insert(kind);
            }
        }

        let mut files = Vec::with_capacity(paths.len());
        for (path, expected_kind) in paths {
            let absolute = worktree_path(&self.workdir, &path)?;
            let metadata = match fs::symlink_metadata(&absolute) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let actual_kind = kind_from_metadata(&metadata)?;
            let kind = if expected_kind == FileKind::Executable && actual_kind == FileKind::Regular
            {
                FileKind::Executable
            } else {
                actual_kind
            };
            let size = if kind == FileKind::Symlink {
                0
            } else {
                metadata.len()
            };
            if size > options.max_file_bytes {
                return Err(Error::UnsupportedRepository(format!(
                    "eligible file {:?} is {size} bytes, over the {} byte safety limit",
                    BString::from(path),
                    options.max_file_bytes
                )));
            }
            let bytes = if kind == FileKind::Symlink {
                fs::read_link(&absolute)?.as_os_str().as_bytes().to_vec()
            } else {
                fs::read(&absolute)?
            };
            files.push(FileDraft {
                path: BString::from(path),
                kind,
                bytes,
            });
        }

        Ok(ViewDraft {
            head,
            refs,
            index,
            files,
        })
    }
}

struct ViewDraft {
    head: BString,
    refs: Vec<(BString, BString)>,
    index: Option<Vec<u8>>,
    files: Vec<FileDraft>,
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

fn persist_draft(store: &RecoveryStore, draft: ViewDraft) -> Result<PersistedDraft> {
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

fn capture_refs(git_dir: &Path) -> Result<Vec<(BString, BString)>> {
    let mut refs = Vec::new();
    let refs_root = git_dir.join("refs");
    if refs_root.is_dir() {
        capture_ref_directory(git_dir, &refs_root, &mut refs)?;
    }
    let packed = git_dir.join("packed-refs");
    if packed.is_file() {
        refs.push((
            BString::from(b"packed-refs".to_vec()),
            BString::from(fs::read(packed)?),
        ));
    }
    refs.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(refs)
}

fn capture_ref_directory(
    git_dir: &Path,
    directory: &Path,
    output: &mut Vec<(BString, BString)>,
) -> Result<()> {
    let mut entries = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            capture_ref_directory(git_dir, &entry.path(), output)?;
        } else if file_type.is_file()
            && entry.path().extension().is_none_or(|value| value != "lock")
        {
            let relative = entry
                .path()
                .strip_prefix(git_dir)
                .map_err(|_| Error::CorruptRecovery("reference escaped Git directory".to_owned()))?
                .as_os_str()
                .as_bytes()
                .to_vec();
            output.push((
                BString::from(relative),
                BString::from(fs::read(entry.path())?),
            ));
        }
    }
    Ok(())
}

fn worktree_path(workdir: &Path, relative: &[u8]) -> Result<PathBuf> {
    if relative.is_empty()
        || relative[0] == b'/'
        || relative.split(|byte| *byte == b'/').any(|component| {
            component.is_empty() || component == b"." || component == b".." || component == b".git"
        })
    {
        return Err(Error::UnsupportedRepository(format!(
            "unsafe worktree path {:?}",
            BString::from(relative)
        )));
    }
    Ok(workdir.join(std::ffi::OsStr::from_bytes(relative)))
}

fn kind_from_metadata(metadata: &fs::Metadata) -> Result<FileKind> {
    if metadata.file_type().is_symlink() {
        Ok(FileKind::Symlink)
    } else if metadata.is_file() {
        if metadata.permissions().mode() & 0o111 == 0 {
            Ok(FileKind::Regular)
        } else {
            Ok(FileKind::Executable)
        }
    } else {
        Err(Error::UnsupportedRepository(
            "special files and directories cannot be captured as files".to_owned(),
        ))
    }
}
