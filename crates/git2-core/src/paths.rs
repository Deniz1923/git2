//! Descriptor-relative worktree entry reads that never follow symlinks or parent links.
//!
//! All worktree access during capture goes through one held-open root descriptor. On
//! Linux, directories are opened `O_PATH` for traversal only. Parent directories are
//! opened without following symlinks, leaves are inspected with no-follow metadata calls
//! before any open, and regular-file reads are bounded at read time by the caller's
//! limit, with a final size check so drift during a read fails instead of persisting a
//! truncated prefix.

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bstr::BString;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, fstat, open, openat, readlinkat, statat};
use rustix::io::{Errno, read};

use crate::recovery::FileKind;
use crate::{Error, Result};

/// Read granularity for bounded regular-file capture.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Symlink targets this long cannot be faithful on Linux; anything longer is refused.
const MAX_SYMLINK_TARGET_BYTES: usize = 64 * 1024;

/// One faithfully capturable worktree entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedEntry {
    /// The entry kind observed on disk, never inferred from the index.
    pub kind: FileKind,
    /// Raw entry bytes: file content, or the target path for symlinks.
    pub bytes: Vec<u8>,
}

/// An open, checked worktree root.
pub struct CheckedRoot {
    fd: OwnedFd,
}

/// Flags for opening a directory that is only used as a traversal anchor.
///
/// On Linux, `O_PATH` obtains a traversal-only descriptor: parent directories are never
/// opened for reading, so a directory that is searchable but not readable still works.
#[cfg(target_os = "linux")]
fn directory_flags(follow_root: bool) -> OFlags {
    let mut flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
    if !follow_root {
        flags |= OFlags::NOFOLLOW;
    }
    flags
}

/// Flags for opening a directory that is only used as a traversal anchor.
#[cfg(not(target_os = "linux"))]
fn directory_flags(follow_root: bool) -> OFlags {
    let mut flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NONBLOCK | OFlags::CLOEXEC;
    if !follow_root {
        flags |= OFlags::NOFOLLOW;
    }
    flags
}

impl CheckedRoot {
    /// Open `root` as the base for checked, descriptor-relative entry reads.
    ///
    /// # Errors
    ///
    /// Returns an error if `root` cannot be opened as a directory.
    pub fn open(root: &Path) -> Result<Self> {
        let fd = open(root, directory_flags(true), Mode::empty()).map_err(io::Error::from)?;
        Ok(Self { fd })
    }

    /// Read one worktree-relative byte path relative to the root.
    ///
    /// Parent directories are traversed without following symlinks, a symlink leaf is read
    /// as its target without ever opening the link, and a regular leaf is read through a
    /// descriptor whose own metadata is the last word on kind, mode, and size. A read never
    /// blocks: only regular files are opened, and opens stay non-blocking.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is not a plain relative byte path, crosses a symlinked
    /// parent, is a special file or directory, or exceeds `limit` bytes. Absent paths
    /// return `Ok(None)` so they can be distinguished from access errors.
    pub fn read_entry(&self, path: &[u8], limit: u64) -> Result<Option<CapturedEntry>> {
        let (parents, leaf) = checked_split(path)?;
        let entry = read_at(self.fd.as_fd(), path, &parents, leaf, limit)?;
        let Some(entry) = entry else {
            return Ok(None);
        };
        if entry.kind == FileKind::Symlink && entry.bytes.len() > MAX_SYMLINK_TARGET_BYTES {
            return Err(Error::UnsupportedRepository(format!(
                "symlink {:?} has a target too long to capture",
                BString::from(path)
            )));
        }
        Ok(Some(entry))
    }
}

/// Walk `parents` without following symlinks, then read the leaf entry.
fn read_at(
    directory: BorrowedFd<'_>,
    path: &[u8],
    parents: &[&[u8]],
    leaf: &[u8],
    limit: u64,
) -> Result<Option<CapturedEntry>> {
    let Some((component, rest)) = parents.split_first() else {
        return read_leaf_at(directory, path, leaf, limit);
    };
    let child = openat(
        directory,
        OsStr::from_bytes(component),
        directory_flags(false),
        Mode::empty(),
    );
    match child {
        Ok(child) => read_at(child.as_fd(), path, rest, leaf, limit),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP) => Err(Error::UnsupportedRepository(format!(
            "worktree path {:?} crosses a symlink",
            BString::from(path)
        ))),
        // Linux reports ENOTDIR when a directory-opened component is a symlink (with
        // O_NOFOLLOW) or a regular file; a no-follow stat decides which one it is.
        Err(Errno::NOTDIR) => match statat(
            directory,
            OsStr::from_bytes(component),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_symlink() => {
                Err(Error::UnsupportedRepository(format!(
                    "worktree path {:?} crosses a symlink",
                    BString::from(path)
                )))
            }
            // A regular ancestor cannot contain the path, but unlike a symlinked parent
            // it is no capture-fidelity hazard: the original ENOTDIR access error is
            // kept deliberately rather than recasting the path as absent.
            Ok(_) => Err(Error::Io(io::Error::from(Errno::NOTDIR))),
            Err(error) => Err(Error::Io(io::Error::from(error))),
        },
        Err(error) => Err(Error::Io(io::Error::from(error))),
    }
}

/// Read the leaf entry relative to its open parent directory.
fn read_leaf_at(
    directory: BorrowedFd<'_>,
    path: &[u8],
    leaf: &[u8],
    limit: u64,
) -> Result<Option<CapturedEntry>> {
    let name = OsStr::from_bytes(leaf);
    let stat = match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(Error::Io(io::Error::from(error))),
    };
    let file_type = FileType::from_raw_mode(stat.st_mode);
    if file_type.is_symlink() {
        let target = readlinkat(directory, name, Vec::new()).map_err(io::Error::from)?;
        return Ok(Some(CapturedEntry {
            kind: FileKind::Symlink,
            bytes: target.into_bytes(),
        }));
    }
    if !file_type.is_file() {
        return Err(Error::UnsupportedRepository(
            "special files and directories cannot be captured as files".to_owned(),
        ));
    }
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    );
    let file = match file {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(Error::Io(io::Error::from(error))),
    };
    // Only regular files are ever opened, and O_NONBLOCK keeps even a raced-in FIFO from
    // blocking; the descriptor's own metadata decides what is actually being read.
    let stat = fstat(file.as_fd()).map_err(io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(Error::UnsupportedRepository(
            "special files and directories cannot be captured as files".to_owned(),
        ));
    }
    let size = u64::try_from(stat.st_size).unwrap_or(u64::MAX);
    if size > limit {
        return Err(Error::UnsupportedRepository(format!(
            "eligible file {:?} is {size} bytes, over the {limit} byte safety limit",
            BString::from(path)
        )));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(usize::try_from(size).unwrap_or(0))
        .map_err(|_| Error::Io(io::Error::other("cannot reserve the capture buffer")))?;
    let mut chunk = [0_u8; READ_CHUNK_BYTES];
    let mut read_bytes = 0_u64;
    while read_bytes < size {
        let want =
            usize::try_from((size - read_bytes).min(chunk.len() as u64)).unwrap_or(chunk.len());
        let read_now = read(file.as_fd(), &mut chunk[..want]).map_err(io::Error::from)?;
        if read_now == 0 {
            break;
        }
        read_bytes += read_now as u64;
        bytes.extend_from_slice(&chunk[..read_now]);
    }
    // Refuse size drift in either direction instead of persisting a truncated prefix:
    // a short read means the file shrank mid-capture, and a size that grew afterwards
    // means the bytes read are no longer the whole file.
    let final_stat = fstat(file.as_fd()).map_err(io::Error::from)?;
    if read_bytes != size || u64::try_from(final_stat.st_size).unwrap_or(u64::MAX) != size {
        return Err(Error::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            format!(
                "eligible file {:?} changed size while being captured",
                BString::from(path)
            ),
        )));
    }
    let kind = if stat.st_mode & 0o111 == 0 {
        FileKind::Regular
    } else {
        FileKind::Executable
    };
    Ok(Some(CapturedEntry { kind, bytes }))
}

/// Split a worktree-relative byte path into validated parent components and its leaf name.
fn checked_split(path: &[u8]) -> Result<(Vec<&[u8]>, &[u8])> {
    let mut components: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
    // Splitting any slice yields at least one component, so the `None` arm below is
    // unreachable; it only exists to keep this function total without panicking.
    let Some(leaf) = components.pop() else {
        return Err(unsafe_path_error(path));
    };
    if is_unsafe_component(leaf) || components.iter().copied().any(is_unsafe_component) {
        return Err(unsafe_path_error(path));
    }
    Ok((components, leaf))
}

/// One path component is unusable when empty, relative, or reserved Git metadata.
fn is_unsafe_component(component: &[u8]) -> bool {
    component.is_empty() || component == b"." || component == b".." || component == b".git"
}

/// The refusal used for any path that is not a plain worktree-relative byte path.
fn unsafe_path_error(path: &[u8]) -> Error {
    Error::UnsupportedRepository(format!("unsafe worktree path {:?}", BString::from(path)))
}
