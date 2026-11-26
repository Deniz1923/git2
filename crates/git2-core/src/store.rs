//! Loose-object storage with packed-object reads delegated to gix.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::object::{GitOid, ObjectKind, RawObject, canonical_object_parts};
use crate::{Error, Result};

#[cfg(unix)]
mod packed;

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

    pub(crate) fn from_repository(repository: gix::Repository) -> Result<Self> {
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
        Ok(RawObject {
            kind: from_gix_kind(object.kind),
            data: object.data.clone(),
        })
    }

    /// Use only local loose and indexed pack files, without replacement refs.
    #[cfg(unix)]
    pub(crate) fn bounded_reader(&self, limit: usize) -> Result<BoundedReader> {
        let objects_dir = self.git_dir.join("objects");
        if !fs::symlink_metadata(&objects_dir)?.is_dir() {
            return Err(Error::UnsupportedRepository(
                "Git objects must live in a real objects directory".to_owned(),
            ));
        }
        Ok(BoundedReader {
            root: crate::paths::CheckedRoot::open(&objects_dir)?,
            objects_dir,
            packs: None,
            limit,
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

pub(crate) fn from_gix_kind(kind: gix::objs::Kind) -> ObjectKind {
    match kind {
        gix::objs::Kind::Blob => ObjectKind::Blob,
        gix::objs::Kind::Tree => ObjectKind::Tree,
        gix::objs::Kind::Commit => ObjectKind::Commit,
        gix::objs::Kind::Tag => ObjectKind::Tag,
    }
}

#[cfg(unix)]
pub(crate) struct BoundedReader {
    root: crate::paths::CheckedRoot,
    objects_dir: PathBuf,
    packs: Option<packed::PackedObjects>,
    limit: usize,
}

#[cfg(unix)]
impl BoundedReader {
    pub(crate) fn read(&mut self, id: GitOid) -> Result<RawObject> {
        let hex = id.to_hex();
        let path = format!("{}/{}", &hex[..2], &hex[2..]);
        // Allow zlib framing/expansion overhead for incompressible payloads.
        let compressed_limit = (self.limit + 1024 * 1024) as u64;
        if let Some(entry) = self.root.read_entry(path.as_bytes(), compressed_limit)? {
            if entry.kind == crate::recovery::FileKind::Symlink {
                return Err(Error::CorruptObject(format!(
                    "loose object {id} is a symlink"
                )));
            }
            return decode_loose(entry.bytes.as_slice(), id, Some(self.limit));
        }
        if gix::hash::ObjectId::from(id.as_bytes()).is_empty_tree() {
            return Ok(RawObject {
                kind: ObjectKind::Tree,
                data: Vec::new(),
            });
        }
        if self.packs.is_none() {
            self.packs = Some(packed::PackedObjects::open(&self.objects_dir, self.limit)?);
        }
        let object = self
            .packs
            .as_ref()
            .ok_or(Error::ObjectNotFound(id))?
            .read(id)?
            .ok_or(Error::ObjectNotFound(id))?;
        verify_object_id(&object, id)?;
        Ok(object)
    }
}

fn read_loose(path: &Path, expected: GitOid) -> Result<RawObject> {
    let file = File::open(path)?;
    decode_loose(file, expected, None)
}

fn decode_loose(input: impl Read, expected: GitOid, limit: Option<usize>) -> Result<RawObject> {
    let mut decoder = ZlibDecoder::new(input);
    let mut canonical = Vec::new();
    if let Some(limit) = limit {
        let canonical_limit = limit + 32;
        decoder
            .take(canonical_limit as u64 + 1)
            .read_to_end(&mut canonical)?;
        if canonical.len() > canonical_limit {
            return Err(object_too_large(expected, limit));
        }
    } else {
        decoder.read_to_end(&mut canonical)?;
    }
    let (kind, data) = canonical_object_parts(&canonical)?;
    if let Some(limit) = limit
        && data.len() > limit
    {
        return Err(object_too_large(expected, limit));
    }
    let object = RawObject {
        kind,
        data: data.to_vec(),
    };
    verify_object_id(&object, expected)?;
    Ok(object)
}

fn object_too_large(id: GitOid, limit: usize) -> Error {
    Error::UnsupportedRepository(format!(
        "Git object {id} exceeds the {limit} byte retention limit"
    ))
}

fn verify_object_id(object: &RawObject, expected: GitOid) -> Result<()> {
    let actual = object.id();
    if actual != expected {
        return Err(Error::CorruptObject(format!(
            "Git object {expected} has content that hashes to {actual}"
        )));
    }
    Ok(())
}

fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_retention_refuses_payloads_over_its_budget() {
        let object = RawObject {
            kind: ObjectKind::Blob,
            data: vec![b'x'; 256],
        };
        let compressed = compress(&object.canonical_bytes()).unwrap();
        assert!(matches!(
            decode_loose(compressed.as_slice(), object.id(), Some(16)),
            Err(Error::UnsupportedRepository(_))
        ));
        assert_eq!(
            decode_loose(compressed.as_slice(), object.id(), Some(256)).unwrap(),
            object
        );
    }
}
