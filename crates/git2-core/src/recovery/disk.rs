//! Owner-private, descriptor-relative storage for recovery records.

use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{
    AtFlags, FileType, Mode, OFlags, Stat, fstat, linkat, mkdirat, open, openat, renameat, statat,
    unlinkat,
};
use rustix::io::Errno;
use rustix::process::geteuid;

use crate::{Error, Result};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const FILE_FLAGS: OFlags = OFlags::NOFOLLOW
    .union(OFlags::NONBLOCK)
    .union(OFlags::CLOEXEC);
const DIR_MODE: Mode = Mode::RUSR.union(Mode::WUSR).union(Mode::XUSR);
const FILE_MODE: Mode = Mode::RUSR.union(Mode::WUSR);

#[derive(Debug, Clone)]
pub(super) struct Directory(Arc<File>);

impl Directory {
    pub(super) fn open_git_dir(path: &Path) -> Result<Self> {
        Ok(Self(Arc::new(File::from(io_result(open(
            path,
            DIR_FLAGS,
            Mode::empty(),
        ))?))))
    }

    pub(super) fn open_child(&self, name: &str, strict: bool) -> Result<Self> {
        let fd = io_result(openat(&*self.0, name, DIR_FLAGS, Mode::empty()))?;
        let file = File::from(fd);
        check_permissions(&io_result(fstat(&file))?, strict, name)?;
        Ok(Self(Arc::new(file)))
    }

    pub(super) fn require_private(&self) -> Result<()> {
        check_permissions(&io_result(fstat(&*self.0))?, true, "git2")
    }

    pub(super) fn create_child(&self, name: &str, exclusive: bool) -> Result<Self> {
        match mkdirat(&*self.0, name, DIR_MODE) {
            Ok(()) => {}
            Err(Errno::EXIST) if !exclusive => {}
            Err(error) => return Err(io::Error::from(error).into()),
        }
        let dir = self.open_child(name, true)?;
        // Sync the parent even when the directory pre-existed: an earlier attempt
        // may have created it but failed before syncing its directory entry.
        self.0.sync_all()?;
        Ok(dir)
    }

    pub(super) fn exists(&self, name: &str) -> Result<bool> {
        match statat(&*self.0, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(error) => Err(io::Error::from(error).into()),
        }
    }

    pub(super) fn read(&self, name: &str, limit: usize) -> Result<Vec<u8>> {
        self.read_with_permissions(name, limit, true)
    }

    pub(super) fn read_with_permissions(
        &self,
        name: &str,
        limit: usize,
        strict: bool,
    ) -> Result<Vec<u8>> {
        let file = self.open_regular(name, OFlags::RDONLY, strict)?;
        let length = file.metadata()?.len();
        let limit = limit as u64;
        if length > limit {
            return Err(too_large(name, limit));
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(too_large(name, limit));
        }
        Ok(bytes)
    }

    pub(super) fn open_lock(&self) -> Result<File> {
        const NAME: &str = "lock";
        match self.create_file(NAME, OFlags::RDWR) {
            Ok(file) => {
                file.sync_all()?;
                self.0.sync_all()?;
                Ok(file)
            }
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                self.open_regular(NAME, OFlags::RDWR, true)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn sync(&self) -> Result<()> {
        self.0.sync_all()?;
        Ok(())
    }

    pub(super) fn publish_new(&self, name: &str, bytes: &[u8]) -> Result<bool> {
        if self.exists(name)? {
            return Ok(false);
        }
        let temp = self.stage(bytes)?;
        let published = match linkat(&*self.0, &temp.name, &*self.0, name, AtFlags::empty()) {
            Ok(()) => true,
            Err(Errno::EXIST) => false,
            Err(error) => return Err(io::Error::from(error).into()),
        };
        temp.remove()?;
        if published {
            self.0.sync_all()?;
        }
        Ok(published)
    }

    pub(super) fn write_immutable(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if self.exists(name)? {
            self.verify_existing(name, bytes)?;
        } else {
            let temp = self.stage(bytes)?;
            match linkat(&*self.0, &temp.name, &*self.0, name, AtFlags::empty()) {
                Ok(()) => {}
                Err(Errno::EXIST) => self.verify_existing(name, bytes)?,
                Err(error) => return Err(io::Error::from(error).into()),
            }
            temp.remove()?;
        }
        self.0.sync_all()?;
        Ok(())
    }

    pub(super) fn write_pointer(&self, name: &str, bytes: &[u8]) -> Result<()> {
        if self.exists(name)? {
            self.open_regular(name, OFlags::RDONLY, true)?;
        }
        let mut temp = self.stage(bytes)?;
        io_result(renameat(&*self.0, &temp.name, &*self.0, name))?;
        temp.active = false;
        self.0.sync_all()?;
        Ok(())
    }

    fn open_regular(&self, name: &str, flags: OFlags, strict: bool) -> Result<File> {
        let stat = io_result(statat(&*self.0, name, AtFlags::SYMLINK_NOFOLLOW))?;
        check_regular(&stat, strict, name)?;
        let fd = io_result(openat(&*self.0, name, FILE_FLAGS | flags, Mode::empty()))?;
        let file = File::from(fd);
        check_regular(&io_result(fstat(&file))?, strict, name)?;
        Ok(file)
    }

    fn create_file(&self, name: &str, flags: OFlags) -> Result<File> {
        let fd = io_result(openat(
            &*self.0,
            name,
            FILE_FLAGS | flags | OFlags::CREATE | OFlags::EXCL,
            FILE_MODE,
        ))?;
        Ok(File::from(fd))
    }

    fn stage(&self, bytes: &[u8]) -> Result<StagedFile<'_>> {
        loop {
            let number = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!("tmp_git2_{}_{number}", std::process::id());
            match self.create_file(&name, OFlags::WRONLY) {
                Ok(mut file) => {
                    let temp = StagedFile {
                        dir: self,
                        name,
                        active: true,
                    };
                    file.write_all(bytes)?;
                    file.sync_all()?;
                    return Ok(temp);
                }
                Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn verify_existing(&self, name: &str, expected: &[u8]) -> Result<()> {
        let mut file = self.open_regular(name, OFlags::RDONLY, true)?;
        if file.metadata()?.len() != expected.len() as u64 || !compare_bytes(&mut file, expected)? {
            return Err(Error::CorruptRecovery(format!(
                "immutable record {name} has unexpected content"
            )));
        }
        Ok(())
    }
}

struct StagedFile<'a> {
    dir: &'a Directory,
    name: String,
    active: bool,
}

impl StagedFile<'_> {
    fn remove(mut self) -> Result<()> {
        io_result(unlinkat(&*self.dir.0, &self.name, AtFlags::empty()))?;
        self.active = false;
        Ok(())
    }
}

impl Drop for StagedFile<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = unlinkat(&*self.dir.0, &self.name, AtFlags::empty());
        }
    }
}

fn check_regular(stat: &Stat, strict: bool, name: &str) -> Result<()> {
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(Error::CorruptRecovery(format!(
            "recovery entry {name} is not a regular file"
        )));
    }
    check_permissions(stat, strict, name)
}

fn check_permissions(stat: &Stat, strict: bool, name: &str) -> Result<()> {
    if stat.st_uid != geteuid().as_raw() || (strict && stat.st_mode & 0o077 != 0) {
        return Err(Error::UnsupportedRepository(format!(
            "recovery entry {name} must be owned by the current user and private to that user"
        )));
    }
    Ok(())
}

fn compare_bytes(file: &mut File, expected: &[u8]) -> Result<bool> {
    let mut buffer = [0; 8192];
    for chunk in expected.chunks(buffer.len()) {
        let actual = &mut buffer[..chunk.len()];
        match file.read_exact(actual) {
            Ok(()) if actual == chunk => {}
            Ok(()) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(false),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(file.read(&mut buffer[..1])? == 0)
}

fn too_large(name: &str, limit: u64) -> Error {
    Error::CorruptRecovery(format!(
        "recovery record {name} exceeds the {limit} byte limit"
    ))
}

fn io_result<T>(result: rustix::io::Result<T>) -> Result<T> {
    result.map_err(|error| io::Error::from(error).into())
}
