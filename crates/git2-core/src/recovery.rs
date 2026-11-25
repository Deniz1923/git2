//! Versioned, content-addressed storage for private recovery views.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use bstr::BString;
use sha2::{Digest as _, Sha256};

use crate::{Error, Result};

const FORMAT: &[u8] = b"1\n";
const OPERATION_MAGIC: &[u8] = b"git2-operation\0\x01";
const VIEW_MAGIC: &[u8] = b"git2-view\0\x01";
const DIGEST_LEN: usize = 32;
const DIGEST_HEX_LEN: usize = DIGEST_LEN * 2;
const MAX_RECORDS: usize = 10_000_000;
const MAX_FIELD_BYTES: usize = 16 * 1024 * 1024;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A SHA-256 identifier for bytes held by the recovery store.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId([u8; DIGEST_LEN]);

impl ContentId {
    /// Hash bytes into a recovery-store identifier.
    #[must_use]
    pub fn for_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        let mut output = [0_u8; DIGEST_LEN];
        output.copy_from_slice(&digest);
        Self(output)
    }

    /// Parse a full 64-character hexadecimal identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if the input has the wrong length or is not hexadecimal.
    pub fn from_hex(hex: &[u8]) -> Result<Self> {
        if hex.len() != DIGEST_HEX_LEN {
            return Err(Error::CorruptRecovery(format!(
                "expected {DIGEST_HEX_LEN} hexadecimal characters, got {}",
                hex.len()
            )));
        }
        let mut bytes = [0_u8; DIGEST_LEN];
        for (index, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = (decode_nibble(pair[0])? << 4) | decode_nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }

    /// Format the identifier as lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(DIGEST_HEX_LEN);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ContentId")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for ContentId {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::from_hex(value.as_bytes())
    }
}

fn decode_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::CorruptRecovery(format!(
            "invalid hexadecimal byte 0x{byte:02x}"
        ))),
    }
}

/// The filesystem representation of a captured path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Regular,
    Executable,
    Symlink,
}

impl FileKind {
    const fn code(self) -> u8 {
        match self {
            Self::Regular => 1,
            Self::Executable => 2,
            Self::Symlink => 3,
        }
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::Regular),
            2 => Ok(Self::Executable),
            3 => Ok(Self::Symlink),
            _ => Err(Error::CorruptRecovery(format!(
                "unknown captured file kind {code}"
            ))),
        }
    }
}

/// A working-copy entry in a recovery view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub path: BString,
    pub kind: FileKind,
    pub content: ContentId,
}

/// A captured supported local repository view intended for the future restore engine.
///
/// Restore and undo are not implemented yet; persisting a `View` alone is not a backup proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Raw `.git/HEAD` bytes.
    pub head: BString,
    /// Raw logical reference files, sorted by repository-relative byte path.
    pub refs: Vec<(BString, BString)>,
    /// The raw index stored in the CAS, or no index if one did not exist.
    pub index: Option<ContentId>,
    /// Present tracked and non-ignored untracked worktree entries.
    pub files: Vec<FileRecord>,
}

/// Why a recovery view was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationKind {
    Adopt,
    ExternalGit,
    Git2(BString),
}

/// An immutable transition between two recovery views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub parent: Option<ContentId>,
    pub before: ContentId,
    pub after: ContentId,
    pub kind: OperationKind,
    pub timestamp_seconds: i64,
}

impl Operation {
    /// Encode this operation in the stable v1 binary format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(OPERATION_MAGIC);
        match self.parent {
            Some(parent) => {
                output.push(1);
                output.extend_from_slice(&parent.0);
            }
            None => output.push(0),
        }
        output.extend_from_slice(&self.before.0);
        output.extend_from_slice(&self.after.0);
        output.extend_from_slice(&self.timestamp_seconds.to_le_bytes());
        match &self.kind {
            OperationKind::Adopt => output.push(1),
            OperationKind::ExternalGit => output.push(2),
            OperationKind::Git2(command) => {
                output.push(3);
                write_bytes(&mut output, command.as_ref());
            }
        }
        output
    }

    /// Decode and validate a v1 binary operation.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if the record is truncated or malformed.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = bytes;
        take_exact(&mut input, OPERATION_MAGIC.len())
            .filter(|magic| *magic == OPERATION_MAGIC)
            .ok_or_else(|| Error::CorruptRecovery("invalid operation header".to_owned()))?;
        let parent = match take_byte(&mut input)? {
            0 => None,
            1 => Some(read_content_id(&mut input, "parent operation")?),
            marker => {
                return Err(Error::CorruptRecovery(format!(
                    "invalid parent marker {marker}"
                )));
            }
        };
        let before = read_content_id(&mut input, "before view")?;
        let after = read_content_id(&mut input, "after view")?;
        let timestamp_bytes = take_required(&mut input, size_of::<i64>(), "operation timestamp")?;
        let mut timestamp = [0_u8; size_of::<i64>()];
        timestamp.copy_from_slice(timestamp_bytes);
        let timestamp_seconds = i64::from_le_bytes(timestamp);
        let kind = match take_byte(&mut input)? {
            1 => OperationKind::Adopt,
            2 => OperationKind::ExternalGit,
            3 => OperationKind::Git2(read_bytes(&mut input)?),
            code => {
                return Err(Error::CorruptRecovery(format!(
                    "unknown operation kind {code}"
                )));
            }
        };
        if !input.is_empty() {
            return Err(Error::CorruptRecovery(
                "operation has trailing bytes".to_owned(),
            ));
        }
        Ok(Self {
            parent,
            before,
            after,
            kind,
            timestamp_seconds,
        })
    }
}

impl View {
    /// Encode this view in the stable v1 binary format.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(VIEW_MAGIC);
        write_bytes(&mut output, self.head.as_ref());
        write_count(&mut output, self.refs.len());
        for (name, value) in &self.refs {
            write_bytes(&mut output, name.as_ref());
            write_bytes(&mut output, value.as_ref());
        }
        match self.index {
            Some(id) => {
                output.push(1);
                output.extend_from_slice(&id.0);
            }
            None => output.push(0),
        }
        write_count(&mut output, self.files.len());
        for file in &self.files {
            write_bytes(&mut output, file.path.as_ref());
            output.push(file.kind.code());
            output.extend_from_slice(&file.content.0);
        }
        output
    }

    /// Decode and validate a v1 binary view.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if the record is truncated, oversized, or malformed.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut input = bytes;
        take_exact(&mut input, VIEW_MAGIC.len())
            .filter(|magic| *magic == VIEW_MAGIC)
            .ok_or_else(|| Error::CorruptRecovery("invalid view header".to_owned()))?;
        let head = read_bytes(&mut input)?;
        let reference_count = read_count(&mut input)?;
        let mut refs = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            refs.push((read_bytes(&mut input)?, read_bytes(&mut input)?));
        }
        let index = match take_byte(&mut input)? {
            0 => None,
            1 => {
                let mut digest = [0_u8; DIGEST_LEN];
                digest.copy_from_slice(take_required(&mut input, DIGEST_LEN, "index digest")?);
                Some(ContentId(digest))
            }
            marker => {
                return Err(Error::CorruptRecovery(format!(
                    "invalid index marker {marker}"
                )));
            }
        };
        let file_count = read_count(&mut input)?;
        let mut files = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            let path = read_bytes(&mut input)?;
            let kind = FileKind::from_code(take_byte(&mut input)?)?;
            let mut digest = [0_u8; DIGEST_LEN];
            digest.copy_from_slice(take_required(&mut input, DIGEST_LEN, "file digest")?);
            files.push(FileRecord {
                path,
                kind,
                content: ContentId(digest),
            });
        }
        if !input.is_empty() {
            return Err(Error::CorruptRecovery("view has trailing bytes".to_owned()));
        }
        Ok(Self {
            head,
            refs,
            index,
            files,
        })
    }
}

/// The private recovery database rooted at `.git/git2`.
#[derive(Debug, Clone)]
pub struct RecoveryStore {
    root: PathBuf,
}

/// An exclusive advisory lock for one recovery database.
pub struct RecoveryLock {
    file: File,
}

impl Drop for RecoveryLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

impl RecoveryStore {
    /// Initialize or open the recovery database under `git_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be created or its format is unsupported.
    pub fn initialize(git_dir: impl AsRef<Path>) -> Result<Self> {
        let root = git_dir.as_ref().join("git2");
        fs::create_dir_all(&root)?;
        let format_path = root.join("format");
        if format_path.exists() {
            let actual = fs::read(&format_path)?;
            if actual != FORMAT {
                return Err(Error::UnsupportedRepository(format!(
                    "unsupported git2 recovery format {:?}",
                    BString::from(actual)
                )));
            }
        } else {
            write_atomic(&format_path, FORMAT)?;
        }
        for directory in ["cas", "operations", "pending", "views"] {
            fs::create_dir_all(root.join(directory))?;
        }
        Ok(Self { root })
    }

    /// Open an existing recovery database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database is absent, unreadable, or uses an unsupported format.
    pub fn open(git_dir: impl AsRef<Path>) -> Result<Self> {
        let root = git_dir.as_ref().join("git2");
        let actual = fs::read(root.join("format"))?;
        if actual != FORMAT {
            return Err(Error::UnsupportedRepository(format!(
                "unsupported git2 recovery format {:?}",
                BString::from(actual)
            )));
        }
        Ok(Self { root })
    }

    /// Return the root of the private database.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Acquire the database's exclusive advisory lock.
    ///
    /// # Errors
    ///
    /// Returns an error immediately if another git2 process holds the lock.
    pub fn try_lock(&self) -> Result<RecoveryLock> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("lock"))?;
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(RecoveryLock { file })
    }

    /// Store content once and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the content cannot be persisted.
    pub fn put_content(&self, bytes: &[u8]) -> Result<ContentId> {
        let id = ContentId::for_bytes(bytes);
        write_immutable(&self.id_path("cas", id), bytes)?;
        Ok(id)
    }

    /// Read content and verify that it still matches its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the content is absent, unreadable, or corrupt.
    pub fn read_content(&self, id: ContentId) -> Result<Vec<u8>> {
        let bytes = fs::read(self.id_path("cas", id))?;
        verify_id(id, &bytes, "content")?;
        Ok(bytes)
    }

    /// Persist an immutable view and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the view cannot be persisted.
    pub fn put_view(&self, view: &View) -> Result<ContentId> {
        let bytes = view.encode();
        let id = ContentId::for_bytes(&bytes);
        write_immutable(&self.id_path("views", id), &bytes)?;
        Ok(id)
    }

    /// Read and validate an immutable view.
    ///
    /// # Errors
    ///
    /// Returns an error if the view is absent, corrupt, or cannot be decoded.
    pub fn read_view(&self, id: ContentId) -> Result<View> {
        let bytes = fs::read(self.id_path("views", id))?;
        verify_id(id, &bytes, "view")?;
        View::decode(&bytes)
    }

    /// Persist an immutable operation and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation cannot be persisted.
    pub fn put_operation(&self, operation: &Operation) -> Result<ContentId> {
        let bytes = operation.encode();
        let id = ContentId::for_bytes(&bytes);
        write_immutable(&self.id_path("operations", id), &bytes)?;
        Ok(id)
    }

    /// Read and validate an immutable operation.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is absent, corrupt, or cannot be decoded.
    pub fn read_operation(&self, id: ContentId) -> Result<Operation> {
        let bytes = fs::read(self.id_path("operations", id))?;
        verify_id(id, &bytes, "operation")?;
        Operation::decode(&bytes)
    }

    /// Atomically update the selected operation.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation does not exist or the pointer cannot be persisted.
    pub fn set_head(&self, id: ContentId) -> Result<()> {
        if !self.id_path("operations", id).is_file() {
            return Err(Error::CorruptRecovery(format!(
                "cannot select missing operation {id}"
            )));
        }
        let mut value = id.to_hex().into_bytes();
        value.push(b'\n');
        write_atomic(&self.root.join("HEAD"), &value)
    }

    /// Read the currently selected operation identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the pointer is absent or malformed.
    pub fn head(&self) -> Result<ContentId> {
        let value = fs::read(self.root.join("HEAD"))?;
        let hex = value
            .strip_suffix(b"\n")
            .ok_or_else(|| Error::CorruptRecovery("git2 HEAD has no newline".to_owned()))?;
        ContentId::from_hex(hex)
    }

    /// Resolve the view selected by the current operation.
    ///
    /// # Errors
    ///
    /// Returns an error if the HEAD operation is absent or corrupt.
    pub fn current_view(&self) -> Result<ContentId> {
        self.read_operation(self.head()?)
            .map(|operation| operation.after)
    }

    fn id_path(&self, namespace: &str, id: ContentId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join(namespace).join(&hex[..2]).join(&hex[2..])
    }
}

fn verify_id(expected: ContentId, bytes: &[u8], noun: &str) -> Result<()> {
    let actual = ContentId::for_bytes(bytes);
    if actual != expected {
        return Err(Error::CorruptRecovery(format!(
            "{noun} names {expected} but hashes to {actual}"
        )));
    }
    Ok(())
}

fn write_count(output: &mut Vec<u8>, count: usize) {
    output.extend_from_slice(&(count as u64).to_le_bytes());
}

fn write_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

fn read_count(input: &mut &[u8]) -> Result<usize> {
    let count = read_u64(input)?;
    let count = usize::try_from(count).map_err(|_| {
        Error::CorruptRecovery("record count does not fit this platform".to_owned())
    })?;
    if count > MAX_RECORDS {
        return Err(Error::CorruptRecovery(format!(
            "record count {count} exceeds safety limit"
        )));
    }
    Ok(count)
}

fn read_bytes(input: &mut &[u8]) -> Result<BString> {
    let length = read_u64(input)?;
    let length = usize::try_from(length).map_err(|_| {
        Error::CorruptRecovery("field length does not fit this platform".to_owned())
    })?;
    if length > MAX_FIELD_BYTES {
        return Err(Error::CorruptRecovery(format!(
            "field length {length} exceeds safety limit"
        )));
    }
    Ok(BString::from(
        take_required(input, length, "byte field")?.to_vec(),
    ))
}

fn read_u64(input: &mut &[u8]) -> Result<u64> {
    let bytes = take_required(input, size_of::<u64>(), "integer")?;
    let mut value = [0_u8; size_of::<u64>()];
    value.copy_from_slice(bytes);
    Ok(u64::from_le_bytes(value))
}

fn read_content_id(input: &mut &[u8], noun: &str) -> Result<ContentId> {
    let mut digest = [0_u8; DIGEST_LEN];
    digest.copy_from_slice(take_required(input, DIGEST_LEN, noun)?);
    Ok(ContentId(digest))
}

fn take_byte(input: &mut &[u8]) -> Result<u8> {
    Ok(take_required(input, 1, "marker")?[0])
}

fn take_required<'a>(input: &mut &'a [u8], length: usize, noun: &str) -> Result<&'a [u8]> {
    take_exact(input, length).ok_or_else(|| Error::CorruptRecovery(format!("truncated {noun}")))
}

fn take_exact<'a>(input: &mut &'a [u8], length: usize) -> Option<&'a [u8]> {
    let (head, tail) = input.split_at_checked(length)?;
    *input = tail;
    Some(head)
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return verify_existing(path, bytes);
    }
    write_atomic(path, bytes)
}

fn verify_existing(path: &Path, expected: &[u8]) -> Result<()> {
    if fs::read(path)? != expected {
        return Err(Error::CorruptRecovery(format!(
            "immutable record at {} has unexpected content",
            path.display()
        )));
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::CorruptRecovery("storage path has no parent".to_owned()))?;
    fs::create_dir_all(parent)?;
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!("tmp_git2_{}_{}", std::process::id(), counter));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    match fs::rename(&temporary, path) {
        Ok(()) => {}
        Err(_error) if path.exists() => {
            let _ = fs::remove_file(&temporary);
            return verify_existing(path, bytes);
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_view(content: ContentId) -> View {
        View {
            head: BString::from(b"ref: refs/heads/main\n".to_vec()),
            refs: vec![(
                BString::from(b"refs/heads/main".to_vec()),
                BString::from(b"0000000000000000000000000000000000000000\n".to_vec()),
            )],
            index: Some(content),
            files: vec![FileRecord {
                path: BString::from(vec![b'n', 0xff]),
                kind: FileKind::Executable,
                content,
            }],
        }
    }

    #[test]
    fn view_round_trips_non_utf8_paths() {
        let view = sample_view(ContentId::for_bytes(b"content"));
        assert_eq!(View::decode(&view.encode()).unwrap(), view);
    }

    #[test]
    fn content_and_views_are_deduplicated_and_verified() {
        let directory = tempfile::tempdir().unwrap();
        let store = RecoveryStore::initialize(directory.path()).unwrap();
        let content = store.put_content(b"same bytes").unwrap();
        assert_eq!(store.put_content(b"same bytes").unwrap(), content);
        assert_eq!(store.read_content(content).unwrap(), b"same bytes");

        let view = sample_view(content);
        let id = store.put_view(&view).unwrap();
        assert_eq!(store.put_view(&view).unwrap(), id);
        assert_eq!(store.read_view(id).unwrap(), view);

        let operation = Operation {
            parent: None,
            before: id,
            after: id,
            kind: OperationKind::Adopt,
            timestamp_seconds: 123,
        };
        let operation_id = store.put_operation(&operation).unwrap();
        assert_eq!(store.read_operation(operation_id).unwrap(), operation);
        store.set_head(operation_id).unwrap();
        assert_eq!(store.head().unwrap(), operation_id);
        assert_eq!(store.current_view().unwrap(), id);
    }

    #[test]
    fn malformed_views_are_rejected() {
        let view = sample_view(ContentId::for_bytes(b"content"));
        let mut encoded = view.encode();
        encoded.pop();
        assert!(matches!(
            View::decode(&encoded),
            Err(Error::CorruptRecovery(_))
        ));
    }

    #[test]
    fn advisory_lock_excludes_a_second_writer() {
        let directory = tempfile::tempdir().unwrap();
        let store = RecoveryStore::initialize(directory.path()).unwrap();
        let first = store.try_lock().unwrap();
        assert!(store.try_lock().is_err());
        drop(first);
        assert!(store.try_lock().is_ok());
    }
}
