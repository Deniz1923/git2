//! Loose-object storage with packed-object reads delegated to gix.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::object::{GitOid, ObjectKind, RawObject, parse_canonical_object};
use crate::{Error, Result};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Access to a Git repository's object database.
pub struct ObjectStore {
    repository: gix::Repository,
    git_dir: PathBuf,
}

impl ObjectStore {
    /// Discover a repository from `path` or one of its parents.
    ///
    /// # Errors
    ///
    /// Returns an error if no repository is found or if it does not use SHA-1 objects.
    pub fn discover(path: impl AsRef<Path>) -> Result<Self> {
        let repository =
            gix::discover(path.as_ref()).map_err(|error| Error::Gix(error.to_string()))?;
        Self::from_repository(repository)
    }

    /// Open a repository at the exact worktree or Git-directory path.
    ///
    /// # Errors
    ///
    /// Returns an error if `path` is not a repository or if it does not use SHA-1 objects.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let repository = gix::open(path.as_ref()).map_err(|error| Error::Gix(error.to_string()))?;
        Self::from_repository(repository)
    }

    fn from_repository(repository: gix::Repository) -> Result<Self> {
        if repository.object_hash() != gix::hash::Kind::Sha1 {
            return Err(Error::UnsupportedRepository(
                "only SHA-1 repositories are supported".to_owned(),
            ));
        }
        let git_dir = repository.path().to_path_buf();
        Ok(Self {
            repository,
            git_dir,
        })
    }

    /// Return the canonical Git directory.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Read an object, using the loose format directly and gix as a packed-object fallback.
    ///
    /// # Errors
    ///
    /// Returns an error if the object is absent, corrupt, or cannot be read.
    pub fn read(&self, id: GitOid) -> Result<RawObject> {
        let loose_path = self.loose_path(id);
        if loose_path.is_file() {
            return read_loose(&loose_path, id);
        }

        let object = self
            .repository
            .find_object(gix::hash::ObjectId::from(id.as_bytes()))
            .map_err(|error| {
                if error.to_string().contains("did not exist")
                    || error.to_string().contains("not found")
                {
                    Error::ObjectNotFound(id)
                } else {
                    Error::Gix(error.to_string())
                }
            })?;
        let kind = match object.kind {
            gix::objs::Kind::Blob => ObjectKind::Blob,
            gix::objs::Kind::Tree => ObjectKind::Tree,
            gix::objs::Kind::Commit => ObjectKind::Commit,
            gix::objs::Kind::Tag => ObjectKind::Tag,
        };
        Ok(RawObject {
            kind,
            data: object.data.clone(),
        })
    }

    /// Write a canonical loose object atomically and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the object cannot be compressed or persisted.
    pub fn write(&self, object: &RawObject) -> Result<GitOid> {
        let id = object.id();
        let target = self.loose_path(id);
        if target.is_file() {
            return Ok(id);
        }

        let parent = target
            .parent()
            .ok_or_else(|| Error::CorruptObject("loose object path has no parent".to_owned()))?;
        fs::create_dir_all(parent)?;

        let compressed = compress(&object.canonical_bytes())?;
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!("tmp_obj_{}_{}", std::process::id(), counter));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&compressed)?;
        file.sync_all()?;
        drop(file);

        match fs::rename(&temporary, &target) {
            Ok(()) => {}
            Err(error) if target.is_file() => {
                let _ = fs::remove_file(&temporary);
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                ) {
                    return Err(error.into());
                }
            }
            Err(error) => {
                let _ = fs::remove_file(&temporary);
                return Err(error.into());
            }
        }

        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(id)
    }

    fn loose_path(&self, id: GitOid) -> PathBuf {
        let hex = id.to_hex();
        self.git_dir.join("objects").join(&hex[..2]).join(&hex[2..])
    }
}

fn read_loose(path: &Path, expected: GitOid) -> Result<RawObject> {
    let file = File::open(path)?;
    let mut decoder = ZlibDecoder::new(file);
    let mut canonical = Vec::new();
    decoder.read_to_end(&mut canonical)?;
    let object = parse_canonical_object(&canonical)?;
    let actual = object.id();
    if actual != expected {
        return Err(Error::CorruptObject(format!(
            "loose object path names {expected} but content hashes to {actual}"
        )));
    }
    Ok(object)
}

fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}
