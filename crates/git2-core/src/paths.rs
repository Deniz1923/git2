//! Descriptor-relative worktree entry reads that never follow symlinks or parent links.
//!
//! All worktree access during capture goes through one held-open root descriptor. On
//! Linux, directories are opened `O_PATH` for traversal only. Parent directories are
//! opened without following symlinks, leaves are inspected with no-follow metadata calls
//! before any open, and regular-file reads are bounded at read time by the caller's
//! limit, with a final size check so drift during a read fails instead of persisting a
//! truncated prefix.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use bstr::BString;
use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, fstat, fsync, mkdirat, open, openat, readlinkat, renameat,
    statat, symlinkat, unlinkat,
};
use rustix::io::{Errno, read};

use crate::recovery::FileKind;
use crate::{Error, Result};

/// Read granularity for bounded regular-file capture.
const READ_CHUNK_BYTES: usize = 16 * 1024;

/// Symlink targets this long cannot be faithful on Linux; anything longer is refused.
const MAX_SYMLINK_TARGET_BYTES: usize = 64 * 1024;

/// Filesystem entry classification observed without following symlinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathEntryKind {
    Regular,
    Executable,
    Symlink,
    Directory,
    Other,
}

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

    /// Read a regular file entry (refusing symlinks and directories) relative to the root.
    pub(crate) fn read_file_raw(&self, path: &[u8], limit: u64) -> Result<Option<Vec<u8>>> {
        let Some(entry) = self.read_entry(path, limit)? else {
            return Ok(None);
        };
        if entry.kind == FileKind::Symlink {
            return Err(Error::UnsupportedRepository(format!(
                "path {:?} must not be a symlink",
                BString::from(path)
            )));
        }
        Ok(Some(entry.bytes))
    }

    /// Inspect `path` without following a final symlink or any symlinked parent.
    pub(crate) fn entry_kind(&self, path: &[u8]) -> Result<Option<PathEntryKind>> {
        let (parents, leaf) = checked_split(path)?;
        let Some(parent) = traverse_parents_ro(self.fd.as_fd(), path, &parents, false)? else {
            return Ok(None);
        };
        let stat = match statat(
            parent.as_fd(),
            OsStr::from_bytes(leaf),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(io::Error::from(error).into()),
        };
        let file_type = FileType::from_raw_mode(stat.st_mode);
        let kind = if file_type.is_symlink() {
            PathEntryKind::Symlink
        } else if file_type.is_dir() {
            PathEntryKind::Directory
        } else if file_type.is_file() {
            if stat.st_mode & 0o111 == 0 {
                PathEntryKind::Regular
            } else {
                PathEntryKind::Executable
            }
        } else {
            PathEntryKind::Other
        };
        Ok(Some(kind))
    }

    /// Inspect `(st_dev, st_ino)` for `path` without following symlinks, refusing non-regular files.
    pub(crate) fn stat_regular_dev_ino(&self, path: &[u8]) -> Result<Option<(u64, u64)>> {
        let (parents, leaf) = checked_split(path)?;
        let Some(parent) = traverse_parents_ro(self.fd.as_fd(), path, &parents, false)? else {
            return Ok(None);
        };
        let stat = match statat(
            parent.as_fd(),
            OsStr::from_bytes(leaf),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(io::Error::from(error).into()),
        };
        if !FileType::from_raw_mode(stat.st_mode).is_file() {
            return Err(Error::CorruptRecovery(format!(
                "lock entry {:?} is not a regular file",
                BString::from(path)
            )));
        }
        Ok(Some((stat.st_dev, stat.st_ino)))
    }

    /// Open the validated parent directory for `path` with `O_RDONLY` (creating parents if requested).
    pub(crate) fn open_parent_dir(
        &self,
        path: &[u8],
        create_missing: bool,
    ) -> Result<(OwnedFd, Vec<u8>)> {
        let (parents, leaf) = checked_split(path)?;
        let parent = traverse_parents_rw(self.fd.as_fd(), path, &parents, create_missing)?
            .ok_or_else(|| {
                Error::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("missing parent directory for {:?}", BString::from(path)),
                ))
            })?;
        Ok((parent, leaf.to_vec()))
    }

    /// Ensure `path` exists as a real directory without following symlinks.
    pub(crate) fn ensure_dir(&self, path: &[u8]) -> Result<()> {
        let (parents, leaf) = checked_split(path)?;
        let mut components = parents;
        components.push(leaf);
        let mut current = open_dot_rw(self.fd.as_fd())?;
        for component in components {
            current = open_or_create_dir(current.as_fd(), path, component, true)?
                .expect("create_missing=true either succeeds or returns Err");
        }
        Ok(())
    }

    /// Remove an empty directory at `path` if it exists; absent paths succeed idempotently.
    pub(crate) fn remove_empty_dir(&self, path: &[u8]) -> Result<()> {
        let (parents, leaf) = checked_split(path)?;
        let Some(parent) = traverse_parents_rw(self.fd.as_fd(), path, &parents, false)? else {
            return Ok(());
        };
        let leaf_os = OsStr::from_bytes(leaf);
        match statat(parent.as_fd(), leaf_os, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_dir() => {}
            Ok(_) => {
                return Err(Error::UnsupportedRepository(format!(
                    "expected directory at {:?}, found non-directory",
                    BString::from(path)
                )));
            }
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(io::Error::from(error).into()),
        }
        match unlinkat(parent.as_fd(), leaf_os, AtFlags::REMOVEDIR) {
            Ok(()) => {}
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(io::Error::from(error).into()),
        }
        fsync(parent.as_fd()).map_err(io::Error::from)?;
        Ok(())
    }

    /// Atomically write or replace a regular file, executable, or symlink at `path`.
    pub(crate) fn write_entry_atomic(
        &self,
        path: &[u8],
        kind: FileKind,
        bytes: &[u8],
        temp_name: &str,
    ) -> Result<()> {
        if is_unsafe_component(temp_name.as_bytes()) || temp_name.contains('/') {
            return Err(unsafe_path_error(temp_name.as_bytes()));
        }
        if kind == FileKind::Symlink && bytes.len() > MAX_SYMLINK_TARGET_BYTES {
            return Err(Error::UnsupportedRepository(format!(
                "symlink {:?} has a target too long to write",
                BString::from(path)
            )));
        }
        let (parent, leaf) = self.open_parent_dir(path, true)?;
        let leaf_os = OsStr::from_bytes(&leaf);
        if let Ok(stat) = statat(parent.as_fd(), leaf_os, AtFlags::SYMLINK_NOFOLLOW)
            && FileType::from_raw_mode(stat.st_mode).is_dir()
        {
            return Err(Error::UnsupportedRepository(format!(
                "cannot replace directory {:?} with a file or symlink",
                BString::from(path)
            )));
        }
        let temp_os = OsStr::new(temp_name);
        let _ = unlinkat(parent.as_fd(), temp_os, AtFlags::empty());
        let mut guard = TempEntryGuard {
            parent: parent.as_fd(),
            name: temp_os,
            active: false,
        };
        match kind {
            FileKind::Regular | FileKind::Executable => {
                let mode = if kind == FileKind::Executable {
                    Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH
                } else {
                    Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::ROTH
                };
                let fd = openat(
                    parent.as_fd(),
                    temp_os,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    mode,
                )
                .map_err(io::Error::from)?;
                guard.active = true;
                let mut file = File::from(fd);
                file.write_all(bytes)?;
                file.sync_all()?;
                drop(file);
            }
            FileKind::Symlink => {
                symlinkat(OsStr::from_bytes(bytes), parent.as_fd(), temp_os)
                    .map_err(io::Error::from)?;
                guard.active = true;
            }
        }
        renameat(parent.as_fd(), temp_os, parent.as_fd(), leaf_os).map_err(io::Error::from)?;
        guard.active = false;
        fsync(parent.as_fd()).map_err(io::Error::from)?;
        Ok(())
    }

    /// Remove a regular file or symlink at `path`; absent paths succeed idempotently.
    pub(crate) fn remove_entry(&self, path: &[u8]) -> Result<()> {
        let (parents, leaf) = checked_split(path)?;
        let Some(parent) = traverse_parents_rw(self.fd.as_fd(), path, &parents, false)? else {
            return Ok(());
        };
        let leaf_os = OsStr::from_bytes(leaf);
        match statat(parent.as_fd(), leaf_os, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_dir() => {
                return Err(Error::UnsupportedRepository(format!(
                    "cannot remove directory {:?} as a file entry",
                    BString::from(path)
                )));
            }
            Ok(_) => {}
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(io::Error::from(error).into()),
        }
        match unlinkat(parent.as_fd(), leaf_os, AtFlags::empty()) {
            Ok(()) => {}
            Err(Errno::NOENT) => return Ok(()),
            Err(error) => return Err(io::Error::from(error).into()),
        }
        fsync(parent.as_fd()).map_err(io::Error::from)?;
        Ok(())
    }

    /// Remove a stray temporary sibling `temp_name` in `path`'s parent directory if present.
    pub(crate) fn cleanup_temp(&self, path: &[u8], temp_name: &str) -> Result<()> {
        if is_unsafe_component(temp_name.as_bytes()) || temp_name.contains('/') {
            return Err(unsafe_path_error(temp_name.as_bytes()));
        }
        let (parents, _) = checked_split(path)?;
        let Some(parent) = traverse_parents_rw(self.fd.as_fd(), path, &parents, false)? else {
            return Ok(());
        };
        match unlinkat(parent.as_fd(), OsStr::new(temp_name), AtFlags::empty()) {
            Ok(()) => {
                fsync(parent.as_fd()).map_err(io::Error::from)?;
            }
            Err(Errno::NOENT) => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
        Ok(())
    }
}

struct TempEntryGuard<'a> {
    parent: BorrowedFd<'a>,
    name: &'a OsStr,
    active: bool,
}

impl Drop for TempEntryGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unlinkat(self.parent, self.name, AtFlags::empty());
        }
    }
}

fn open_dot_rw(root: BorrowedFd<'_>) -> Result<OwnedFd> {
    openat(
        root,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| Error::Io(io::Error::from(error)))
}

fn open_or_create_dir(
    parent: BorrowedFd<'_>,
    full_path: &[u8],
    component: &[u8],
    create_missing: bool,
) -> Result<Option<OwnedFd>> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let name = OsStr::from_bytes(component);
    match openat(parent, name, flags, Mode::empty()) {
        Ok(dir) => Ok(Some(dir)),
        Err(Errno::NOENT) if create_missing => {
            let mode = Mode::RWXU | Mode::RGRP | Mode::XGRP | Mode::ROTH | Mode::XOTH;
            match mkdirat(parent, name, mode) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(io::Error::from(error).into()),
            }
            fsync(parent).map_err(io::Error::from)?;
            openat(parent, name, flags, Mode::empty())
                .map(Some)
                .map_err(|error| Error::Io(io::Error::from(error)))
        }
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP) => Err(Error::UnsupportedRepository(format!(
            "worktree path {:?} crosses a symlink",
            BString::from(full_path)
        ))),
        Err(Errno::NOTDIR) => match statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_symlink() => {
                Err(Error::UnsupportedRepository(format!(
                    "worktree path {:?} crosses a symlink",
                    BString::from(full_path)
                )))
            }
            Ok(_) => Err(Error::UnsupportedRepository(format!(
                "path {:?} has a non-directory parent",
                BString::from(full_path)
            ))),
            Err(error) => Err(io::Error::from(error).into()),
        },
        Err(error) => Err(io::Error::from(error).into()),
    }
}

fn traverse_parents_ro(
    root: BorrowedFd<'_>,
    full_path: &[u8],
    parents: &[&[u8]],
    create_missing: bool,
) -> Result<Option<OwnedFd>> {
    if parents.is_empty() {
        return openat(root, ".", directory_flags(true), Mode::empty())
            .map(Some)
            .map_err(|error| Error::Io(io::Error::from(error)));
    }
    if create_missing {
        return traverse_parents_rw(root, full_path, parents, true);
    }
    let mut current = openat(root, ".", directory_flags(true), Mode::empty())
        .map_err(|error| Error::Io(io::Error::from(error)))?;
    for component in parents {
        let name = OsStr::from_bytes(component);
        match openat(current.as_fd(), name, directory_flags(false), Mode::empty()) {
            Ok(next) => current = next,
            Err(Errno::NOENT) => return Ok(None),
            Err(Errno::LOOP) => {
                return Err(Error::UnsupportedRepository(format!(
                    "worktree path {:?} crosses a symlink",
                    BString::from(full_path)
                )));
            }
            Err(Errno::NOTDIR) => match statat(current.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_symlink() => {
                    return Err(Error::UnsupportedRepository(format!(
                        "worktree path {:?} crosses a symlink",
                        BString::from(full_path)
                    )));
                }
                Ok(_) => return Err(Error::Io(io::Error::from(Errno::NOTDIR))),
                Err(error) => return Err(io::Error::from(error).into()),
            },
            Err(error) => return Err(io::Error::from(error).into()),
        }
    }
    Ok(Some(current))
}

fn traverse_parents_rw(
    root: BorrowedFd<'_>,
    full_path: &[u8],
    parents: &[&[u8]],
    create_missing: bool,
) -> Result<Option<OwnedFd>> {
    let Some((last, prefix)) = parents.split_last() else {
        return open_dot_rw(root).map(Some);
    };
    let current = if create_missing {
        let mut dir = open_dot_rw(root)?;
        for component in prefix {
            dir = open_or_create_dir(dir.as_fd(), full_path, component, true)?
                .expect("create_missing=true returns Some or Err");
        }
        dir
    } else {
        let Some(dir) = traverse_parents_ro(root, full_path, prefix, false)? else {
            return Ok(None);
        };
        dir
    };
    open_or_create_dir(current.as_fd(), full_path, last, create_missing)
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
pub(crate) fn checked_split(path: &[u8]) -> Result<(Vec<&[u8]>, &[u8])> {
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
