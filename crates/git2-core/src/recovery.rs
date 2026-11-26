//! Versioned, owner-private, content-addressed storage for recovery views.
//!
//! The database under `.git/git2` is format `2\n`: a private `format` marker, a
//! versioned `policy` record, an advisory `lock`, immutable SHA-256 content under
//! `cas` plus immutable records under `operations` and `views`, a reserved
//! `pending` directory for the future journal, and one atomically replaced `HEAD`
//! pointer. Every filesystem access is descriptor-relative and never follows
//! symlinks, and effective ownership and permission bits are verified along the
//! way. Legacy format 1 databases are refused byte-for-byte untouched rather than
//! migrated or reinterpreted.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use bstr::BString;
use sha2::{Digest as _, Sha256};

use crate::object::GitOid;
use crate::{Error, Result};

mod disk;

use self::disk::Directory;

/// The on-disk format marker this version writes and reads.
const FORMAT: &[u8] = b"2\n";
/// The operation record header, unchanged from format 1.
const OPERATION_MAGIC: &[u8] = b"git2-operation\0\x01";
/// The view record header introduced with format 2.
const VIEW_MAGIC: &[u8] = b"git2-view\0\x02";
/// The versioned capture-policy record header.
const POLICY_MAGIC: &[u8] = b"git2-policy\0\x01";
const DIGEST_LEN: usize = 32;
const DIGEST_HEX_LEN: usize = DIGEST_LEN * 2;
const OID_LEN: usize = 20;
/// Safety cap on any declared record count.
const MAX_RECORDS: usize = 10_000_000;
/// Largest single byte field inside a view record.
pub(crate) const MAX_FIELD_BYTES: usize = 16 * 1024 * 1024;
/// Largest whole view record.
const MAX_VIEW_BYTES: usize = 64 * 1024 * 1024;
/// Largest whole operation record; control records use the same limit.
const MAX_OPERATION_BYTES: usize = 1024 * 1024;
/// Largest single content-addressed payload.
pub(crate) const MAX_CAS_BYTES: usize = 1024 * 1024 * 1024;
/// Smallest possible encoded reference entry: two empty length prefixes.
const REFERENCE_ENTRY_BYTES: usize = size_of::<u64>() * 2;
/// Smallest possible encoded file entry: empty path, kind byte, digest.
const FILE_ENTRY_BYTES: usize = size_of::<u64>() + 1 + DIGEST_LEN;
/// One encoded retained-object map entry: raw object id plus raw content id.
const RETAINED_OBJECT_BYTES: usize = OID_LEN + DIGEST_LEN;

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

/// One entry of a view's reserved object map.
///
/// The map records which Git objects a checkpoint retains, mapping each
/// object id to the content record holding its bytes. It is strictly sorted and
/// free of duplicates. Earlier capture-only format 2 views may have empty maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedObject {
    /// The retained Git object id.
    pub oid: GitOid,
    /// The content record retaining that object's bytes.
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
    /// The reserved v2 object map, strictly sorted by Git object id.
    pub retained_objects: Vec<RetainedObject>,
}

impl View {
    /// Encode this view in the stable v2 binary format.
    ///
    /// The exact encoded size is preflighted with checked arithmetic before any
    /// allocation, so a view beyond the record, field, or map-ordering bounds is
    /// refused without touching storage.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if any field or the whole record exceeds the
    /// encoded-size bounds, or if the retained-object map is not strictly sorted.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let size = self.encoded_size()?;
        let mut output = Vec::with_capacity(size);
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
        write_count(&mut output, self.retained_objects.len());
        for retained in &self.retained_objects {
            output.extend_from_slice(&retained.oid.as_bytes());
            output.extend_from_slice(&retained.content.0);
        }
        debug_assert_eq!(output.len(), size);
        Ok(output)
    }

    /// Decode and validate a v2 binary view.
    ///
    /// Decoding refuses records beyond the whole-view limit, unknown variants,
    /// truncated fields, counts whose minimum encoding cannot fit in the remaining
    /// bytes, retained-object maps that do not strictly increase, and trailing bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if the record is oversized, truncated, or malformed.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_VIEW_BYTES {
            return Err(Error::CorruptRecovery(format!(
                "view record of {} bytes exceeds the {MAX_VIEW_BYTES} byte limit",
                bytes.len()
            )));
        }
        let mut input = bytes;
        take_exact(&mut input, VIEW_MAGIC.len())
            .filter(|magic| *magic == VIEW_MAGIC)
            .ok_or_else(|| Error::CorruptRecovery("invalid view header".to_owned()))?;
        let head = read_bytes(&mut input)?;
        let reference_count = read_count(&mut input)?;
        check_count(
            reference_count,
            REFERENCE_ENTRY_BYTES,
            input.len(),
            "reference",
        )?;
        let mut refs = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            refs.push((read_bytes(&mut input)?, read_bytes(&mut input)?));
        }
        let index = match take_byte(&mut input)? {
            0 => None,
            1 => Some(read_content_id(&mut input, "index digest")?),
            marker => {
                return Err(Error::CorruptRecovery(format!(
                    "invalid index marker {marker}"
                )));
            }
        };
        let file_count = read_count(&mut input)?;
        check_count(file_count, FILE_ENTRY_BYTES, input.len(), "file")?;
        let mut files = Vec::with_capacity(file_count);
        for _ in 0..file_count {
            let path = read_bytes(&mut input)?;
            let kind = FileKind::from_code(take_byte(&mut input)?)?;
            let content = read_content_id(&mut input, "file digest")?;
            files.push(FileRecord {
                path,
                kind,
                content,
            });
        }
        let retained_count = read_count(&mut input)?;
        check_count(
            retained_count,
            RETAINED_OBJECT_BYTES,
            input.len(),
            "retained object",
        )?;
        let mut retained_objects = Vec::with_capacity(retained_count);
        let mut previous: Option<GitOid> = None;
        for _ in 0..retained_count {
            let oid = GitOid::from_bytes(read_fixed::<OID_LEN>(&mut input, "retained object id")?);
            if previous.is_some_and(|value| value >= oid) {
                return Err(Error::CorruptRecovery(
                    "the retained-object map must strictly increase by object id".to_owned(),
                ));
            }
            previous = Some(oid);
            let content = read_content_id(&mut input, "retained content")?;
            retained_objects.push(RetainedObject { oid, content });
        }
        if !input.is_empty() {
            return Err(Error::CorruptRecovery("view has trailing bytes".to_owned()));
        }
        Ok(Self {
            head,
            refs,
            index,
            files,
            retained_objects,
        })
    }

    /// Compute the exact encoded size, checking every bound first.
    fn encoded_size(&self) -> Result<usize> {
        for adjacent in self.retained_objects.windows(2) {
            if adjacent[0].oid >= adjacent[1].oid {
                return Err(Error::CorruptRecovery(
                    "the retained-object map must strictly increase by object id".to_owned(),
                ));
            }
        }
        check_view_size(
            self.head.len(),
            &self.refs,
            self.index.is_some(),
            self.files.iter().map(|file| file.path.len()),
            self.retained_objects.len(),
        )
    }
}

/// Check view bounds even before content IDs have been computed.
pub(crate) fn check_view_size(
    head_len: usize,
    refs: &[(BString, BString)],
    has_index: bool,
    path_lengths: impl Iterator<Item = usize>,
    retained_count: usize,
) -> Result<usize> {
    let mut total = VIEW_MAGIC.len() + 4 * size_of::<u64>() + 1;
    if has_index {
        total += DIGEST_LEN;
    }
    total = add_sizes(total, checked_field(head_len, "view HEAD")?)?;
    for (name, value) in refs {
        total = add_sizes(total, REFERENCE_ENTRY_BYTES)?;
        total = add_sizes(total, checked_field(name.len(), "reference name")?)?;
        total = add_sizes(total, checked_field(value.len(), "reference value")?)?;
    }
    for length in path_lengths {
        total = add_sizes(total, FILE_ENTRY_BYTES)?;
        total = add_sizes(total, checked_field(length, "file path")?)?;
    }
    total = add_sizes(
        total,
        retained_count
            .checked_mul(RETAINED_OBJECT_BYTES)
            .ok_or_else(|| {
                Error::CorruptRecovery(
                    "the retained-object map exceeds the encodable size".to_owned(),
                )
            })?,
    )?;
    if total > MAX_VIEW_BYTES {
        return Err(Error::CorruptRecovery(format!(
            "the encoded view is {total} bytes, above the {MAX_VIEW_BYTES} byte record limit"
        )));
    }
    Ok(total)
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
    ///
    /// The exact encoded size is preflighted with checked arithmetic before any
    /// allocation, so an operation beyond the whole-record limit is refused without
    /// touching storage.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if the encoded record would exceed the
    /// whole-operation limit.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let command: Option<&[u8]> = match &self.kind {
            OperationKind::Git2(command) => Some(command.as_ref()),
            OperationKind::Adopt | OperationKind::ExternalGit => None,
        };
        let mut total = OPERATION_MAGIC.len() + 1 + DIGEST_LEN * 2 + size_of::<i64>() + 1;
        if self.parent.is_some() {
            total += DIGEST_LEN;
        }
        if let Some(command) = command {
            total = add_sizes(total, add_sizes(size_of::<u64>(), command.len())?)?;
        }
        if total > MAX_OPERATION_BYTES {
            return Err(Error::CorruptRecovery(format!(
                "the encoded operation is {total} bytes, above the {MAX_OPERATION_BYTES} byte record limit"
            )));
        }
        let mut output = Vec::with_capacity(total);
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
        debug_assert_eq!(output.len(), total);
        Ok(output)
    }

    /// Decode and validate an operation record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptRecovery`] if the record is oversized, truncated, or malformed.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_OPERATION_BYTES {
            return Err(Error::CorruptRecovery(format!(
                "operation record of {} bytes exceeds the {MAX_OPERATION_BYTES} byte limit",
                bytes.len()
            )));
        }
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

/// Safety limits that govern capturing eligible worktree files.
///
/// The limit is authoritative for later imports too: it is persisted in the
/// private policy record when the store is created, and imports always reuse the
/// persisted value rather than accepting a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapturePolicy {
    /// Refuse to capture any eligible file larger than this many bytes.
    pub max_file_bytes: u64,
}

impl CapturePolicy {
    /// The default eligible-file limit: 100 MiB.
    const DEFAULT_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;
    /// The fixed implementation cap: no configuration may exceed 1 GiB.
    const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

    /// Validate that this policy is within the implementation bounds.
    ///
    /// A zero limit is allowed (it refuses every non-empty eligible file), while
    /// limits above 1 GiB are refused before any Git state is touched.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedRepository`] if the limit exceeds 1 GiB.
    pub fn validate(self) -> Result<Self> {
        if self.max_file_bytes > Self::MAX_FILE_BYTES {
            return Err(Error::UnsupportedRepository(format!(
                "the eligible-file limit {} exceeds the 1 GiB implementation cap",
                self.max_file_bytes
            )));
        }
        Ok(self)
    }
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// The private recovery database rooted at `.git/git2`.
#[derive(Debug, Clone)]
pub struct RecoveryStore {
    root: PathBuf,
    directory: Directory,
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
    /// Create a brand-new format 2 recovery database under `git_dir`.
    ///
    /// Creation is exclusive: any pre-existing `git2` entry is refused without
    /// modifying a byte, and a legacy format 1 store earns the same preservation
    /// advice that opening reports.
    ///
    /// # Errors
    ///
    /// Returns an error if the policy exceeds the implementation bounds, a store
    /// already exists, or the private layout cannot be created durably.
    pub fn initialize(git_dir: impl AsRef<Path>, policy: CapturePolicy) -> Result<Self> {
        let policy = policy.validate()?;
        let handle = Directory::open_git_dir(git_dir.as_ref())?;
        let directory = match handle.create_child("git2", true) {
            Ok(directory) => directory,
            Err(Error::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {
                // Diagnose the existing store so legacy formats stay actionable.
                return match Self::open(git_dir.as_ref()) {
                    Ok(_) => Err(Error::UnsupportedRepository(
                        "this repository already has a git2 recovery store".to_owned(),
                    )),
                    Err(error) => Err(error),
                };
            }
            Err(error) => return Err(error),
        };
        for namespace in ["cas", "operations", "pending", "views"] {
            directory.create_child(namespace, true)?;
        }
        directory.write_immutable("policy", &policy_record(policy))?;
        directory.write_immutable("format", FORMAT)?;
        Ok(Self {
            root: git_dir.as_ref().join("git2"),
            directory,
        })
    }

    /// Report whether a recovery store entry exists under `git_dir`, without following it.
    ///
    /// # Errors
    ///
    /// Returns an error if the Git directory cannot be inspected.
    pub fn exists(git_dir: impl AsRef<Path>) -> Result<bool> {
        Directory::open_git_dir(git_dir.as_ref())?.exists("git2")
    }

    /// Open an existing format 2 recovery database.
    ///
    /// Opening first reads only the format marker with relaxed permission checks,
    /// so a legacy format 1 store earns format-specific preservation advice even
    /// when its old directories and files carry ordinary 0755/0644 modes. For
    /// format 2, open validates the store and its top-level entries. Fanout
    /// directories and records are checked when accessed.
    ///
    /// # Errors
    ///
    /// Returns an error if the database is absent, unsafe, or malformed.
    pub fn open(git_dir: impl AsRef<Path>) -> Result<Self> {
        let handle = Directory::open_git_dir(git_dir.as_ref())?;
        let directory = handle.open_child("git2", false)?;
        let actual = required_entry(
            directory.read_with_permissions("format", 32, false),
            "format",
        )?;
        if actual == b"1\n" {
            return Err(Error::UnsupportedRepository(
                "git2 recovery format 1 is not supported; the store was left untouched. \
                 Archive the entire .git/git2 directory separately before adopting again"
                    .to_owned(),
            ));
        }
        if actual != FORMAT {
            return Err(Error::UnsupportedRepository(format!(
                "unsupported git2 recovery format {:?}; the store was left untouched",
                BString::from(actual)
            )));
        }
        directory.require_private()?;
        let format = required_entry(directory.read("format", FORMAT.len()), "format")?;
        if format != FORMAT {
            return Err(Error::CorruptRecovery(
                "the recovery format marker changed while opening".to_owned(),
            ));
        }
        for namespace in ["cas", "operations", "pending", "views"] {
            required_entry(directory.open_child(namespace, true), namespace)?;
        }
        let store = Self {
            root: git_dir.as_ref().join("git2"),
            directory,
        };
        store.policy()?;
        Ok(store)
    }

    /// Return the root of the private database.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the persisted capture policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the policy record is unreadable or malformed.
    pub fn policy(&self) -> Result<CapturePolicy> {
        let bytes = required_entry(
            self.directory
                .read("policy", POLICY_MAGIC.len() + size_of::<u64>()),
            "policy",
        )?;
        parse_policy(&bytes)
    }

    /// Acquire the database's exclusive advisory lock.
    ///
    /// # Errors
    ///
    /// Returns an error immediately if another git2 process holds the lock.
    pub fn try_lock(&self) -> Result<RecoveryLock> {
        let file = self.directory.open_lock()?;
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(RecoveryLock { file })
    }

    /// Store content once and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the content exceeds the store's payload limit or cannot
    /// be persisted without replacing existing bytes.
    pub fn put_content(&self, bytes: &[u8]) -> Result<ContentId> {
        if bytes.len() > MAX_CAS_BYTES {
            return Err(Error::CorruptRecovery(format!(
                "content of {} bytes exceeds the {MAX_CAS_BYTES} byte store limit",
                bytes.len()
            )));
        }
        let id = ContentId::for_bytes(bytes);
        self.fanout_directory("cas", id, true)?
            .write_immutable(&record_name(id), bytes)?;
        Ok(id)
    }

    /// Read content and verify that it still matches its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the content is absent, unsafe, unreadable, or corrupt.
    pub fn read_content(&self, id: ContentId) -> Result<Vec<u8>> {
        self.read_content_bounded(id, MAX_CAS_BYTES)
    }

    pub(crate) fn read_content_bounded(&self, id: ContentId, limit: usize) -> Result<Vec<u8>> {
        let bytes = self
            .fanout_directory("cas", id, false)?
            .read(&record_name(id), limit.min(MAX_CAS_BYTES))?;
        verify_id(id, &bytes, "content")?;
        Ok(bytes)
    }

    /// Persist an immutable view and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the view cannot be encoded within record bounds or
    /// cannot be persisted without replacing existing bytes.
    pub fn put_view(&self, view: &View) -> Result<ContentId> {
        let bytes = view.encode()?;
        let id = ContentId::for_bytes(&bytes);
        self.fanout_directory("views", id, true)?
            .write_immutable(&record_name(id), &bytes)?;
        Ok(id)
    }

    /// Read and validate an immutable view.
    ///
    /// # Errors
    ///
    /// Returns an error if the view is absent, unsafe, corrupt, or cannot be decoded.
    pub fn read_view(&self, id: ContentId) -> Result<View> {
        let bytes = self
            .fanout_directory("views", id, false)?
            .read(&record_name(id), MAX_VIEW_BYTES)?;
        verify_id(id, &bytes, "view")?;
        View::decode(&bytes)
    }

    /// Persist an immutable operation and return its identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation cannot be encoded within record bounds or
    /// cannot be persisted without replacing existing bytes.
    pub fn put_operation(&self, operation: &Operation) -> Result<ContentId> {
        let bytes = operation.encode()?;
        let id = ContentId::for_bytes(&bytes);
        self.fanout_directory("operations", id, true)?
            .write_immutable(&record_name(id), &bytes)?;
        Ok(id)
    }

    /// Read and validate an immutable operation.
    ///
    /// # Errors
    ///
    /// Returns an error if the operation is absent, unsafe, corrupt, or cannot be decoded.
    pub fn read_operation(&self, id: ContentId) -> Result<Operation> {
        let bytes = self
            .fanout_directory("operations", id, false)?
            .read(&record_name(id), MAX_OPERATION_BYTES)?;
        verify_id(id, &bytes, "operation")?;
        Operation::decode(&bytes)
    }

    /// Atomically publish an operation as the selected HEAD.
    ///
    /// The selected operation, its immediate parent, and its before/after views
    /// must exist and pass decoding and digest checks.
    ///
    /// # Errors
    ///
    /// Returns an error if any of those records are missing or corrupt, or the
    /// pointer cannot be replaced.
    pub fn set_head(&self, id: ContentId) -> Result<()> {
        let operation = self.read_operation(id)?;
        if let Some(parent) = operation.parent {
            self.read_operation(parent)?;
        }
        self.read_view(operation.before)?;
        if operation.after != operation.before {
            self.read_view(operation.after)?;
        }
        let mut value = id.to_hex().into_bytes();
        value.push(b'\n');
        self.directory.write_pointer("HEAD", &value)
    }

    /// Read the currently selected operation identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the pointer is absent or malformed.
    pub fn head(&self) -> Result<ContentId> {
        let value = required_entry(self.directory.read("HEAD", DIGEST_HEX_LEN + 1), "HEAD")?;
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

    /// Open the fanout directory that holds one record, creating it if requested.
    fn fanout_directory(&self, namespace: &str, id: ContentId, create: bool) -> Result<Directory> {
        let hex = id.to_hex();
        let parent = self.directory.open_child(namespace, true)?;
        if create {
            parent.create_child(&hex[..2], false)
        } else {
            parent.open_child(&hex[..2], true)
        }
    }
}

fn required_entry<T>(entry: Result<T>, name: &str) -> Result<T> {
    entry.map_err(|error| match error {
        Error::Io(cause) if cause.kind() == io::ErrorKind::NotFound => Error::CorruptRecovery(
            format!("incomplete .git/git2 store: missing {name}. Archive the entire directory separately before adopting again"),
        ),
        error => error,
    })
}

/// The record's file name inside its fanout directory.
fn record_name(id: ContentId) -> String {
    id.to_hex()[2..].to_owned()
}

/// Encode the versioned, owner-private policy record.
fn policy_record(policy: CapturePolicy) -> Vec<u8> {
    let mut record = Vec::with_capacity(POLICY_MAGIC.len() + size_of::<u64>());
    record.extend_from_slice(POLICY_MAGIC);
    record.extend_from_slice(&policy.max_file_bytes.to_le_bytes());
    record
}

/// Decode and sanity-check the persisted policy record.
fn parse_policy(record: &[u8]) -> Result<CapturePolicy> {
    let expected = POLICY_MAGIC.len() + size_of::<u64>();
    if record.len() != expected {
        return Err(Error::CorruptRecovery(format!(
            "the policy record has {} bytes, expected {expected}",
            record.len()
        )));
    }
    if record[..POLICY_MAGIC.len()] != *POLICY_MAGIC {
        return Err(Error::CorruptRecovery(
            "the policy record has an invalid header".to_owned(),
        ));
    }
    let mut limit = [0_u8; size_of::<u64>()];
    limit.copy_from_slice(&record[POLICY_MAGIC.len()..]);
    let policy = CapturePolicy {
        max_file_bytes: u64::from_le_bytes(limit),
    };
    if policy.max_file_bytes > CapturePolicy::MAX_FILE_BYTES {
        return Err(Error::CorruptRecovery(format!(
            "the persisted eligible-file limit {} exceeds the {} byte bound",
            policy.max_file_bytes,
            CapturePolicy::MAX_FILE_BYTES
        )));
    }
    Ok(policy)
}

/// Add two encoded sizes, refusing the impossible overflow before allocation.
fn add_sizes(total: usize, addend: usize) -> Result<usize> {
    total
        .checked_add(addend)
        .ok_or_else(|| Error::CorruptRecovery("the encoded record size overflows".to_owned()))
}

/// Return one view field's length, refusing fields beyond the per-field cap.
fn checked_field(len: usize, noun: &str) -> Result<usize> {
    if len > MAX_FIELD_BYTES {
        return Err(Error::CorruptRecovery(format!(
            "the {noun} field is {len} bytes, above the {MAX_FIELD_BYTES} byte field limit"
        )));
    }
    Ok(len)
}

/// Refuse a declared count whose minimum encoding cannot fit in the remaining bytes.
fn check_count(count: usize, entry_bytes: usize, remaining: usize, noun: &str) -> Result<()> {
    let required = count.checked_mul(entry_bytes).ok_or_else(|| {
        Error::CorruptRecovery(format!("the {noun} count overflows the record size"))
    })?;
    if required > remaining {
        return Err(Error::CorruptRecovery(format!(
            "the {noun} count {count} needs {required} bytes but only {remaining} remain"
        )));
    }
    Ok(())
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
    Ok(ContentId(read_fixed::<DIGEST_LEN>(input, noun)?))
}

fn read_fixed<const N: usize>(input: &mut &[u8], noun: &str) -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    bytes.copy_from_slice(take_required(input, N, noun)?);
    Ok(bytes)
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
            retained_objects: Vec::new(),
        }
    }

    #[test]
    fn view_round_trips_non_utf8_paths() {
        let view = sample_view(ContentId::for_bytes(b"content"));
        assert_eq!(View::decode(&view.encode().unwrap()).unwrap(), view);
    }

    #[test]
    fn content_and_views_are_deduplicated_and_verified() {
        let directory = tempfile::tempdir().unwrap();
        let store = RecoveryStore::initialize(directory.path(), CapturePolicy::default()).unwrap();
        assert_eq!(store.policy().unwrap(), CapturePolicy::default());
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
        let mut encoded = view.encode().unwrap();
        encoded.pop();
        assert!(matches!(
            View::decode(&encoded),
            Err(Error::CorruptRecovery(_))
        ));
    }

    #[test]
    fn view_truncations_are_refused() {
        let view = sample_view(ContentId::for_bytes(b"content"));
        let encoded = view.encode().unwrap();
        for end in 0..encoded.len() {
            assert!(
                View::decode(&encoded[..end]).is_err(),
                "truncation at {end}"
            );
        }
        assert_eq!(View::decode(&encoded).unwrap(), view);
    }

    #[test]
    fn advisory_lock_excludes_a_second_writer() {
        let directory = tempfile::tempdir().unwrap();
        let store = RecoveryStore::initialize(directory.path(), CapturePolicy::default()).unwrap();
        let first = store.try_lock().unwrap();
        assert!(store.try_lock().is_err());
        drop(first);
        assert!(store.try_lock().is_ok());
    }

    #[test]
    fn retained_objects_round_trip_and_reject_disorder() {
        let mut view = sample_view(ContentId::for_bytes(b"content"));
        view.retained_objects = vec![
            RetainedObject {
                oid: GitOid::from_bytes([0x11; OID_LEN]),
                content: ContentId::for_bytes(b"first"),
            },
            RetainedObject {
                oid: GitOid::from_bytes([0x22; OID_LEN]),
                content: ContentId::for_bytes(b"second"),
            },
        ];
        assert_eq!(View::decode(&view.encode().unwrap()).unwrap(), view);

        let mut descending = view.clone();
        descending.retained_objects.reverse();
        assert!(descending.encode().is_err());

        let mut encoded = view.encode().unwrap();
        let first_oid = encoded.len() - RETAINED_OBJECT_BYTES * 2;
        encoded[first_oid] = 0xff;
        assert!(View::decode(&encoded).is_err());
    }

    #[test]
    fn view_counts_are_bounded_by_remaining_bytes() {
        let mut claimed = VIEW_MAGIC.to_vec();
        claimed.extend_from_slice(&0_u64.to_le_bytes());
        claimed.extend_from_slice(&(MAX_RECORDS as u64).to_le_bytes());
        assert!(View::decode(&claimed).is_err());

        let mut oversize = sample_view(ContentId::for_bytes(b"content"));
        oversize.head = vec![0; MAX_FIELD_BYTES + 1].into();
        assert!(oversize.encode().is_err());
    }

    #[test]
    fn operations_are_bounded_and_reject_unknown_kinds() {
        let id = ContentId::for_bytes(b"view");
        let operation = Operation {
            parent: None,
            before: id,
            after: id,
            kind: OperationKind::Git2(vec![b'x'; MAX_OPERATION_BYTES].into()),
            timestamp_seconds: 0,
        };
        assert!(operation.encode().is_err());

        let mut huge = vec![0; MAX_OPERATION_BYTES + 1];
        huge[..OPERATION_MAGIC.len()].copy_from_slice(OPERATION_MAGIC);
        assert!(Operation::decode(&huge).is_err());

        let adopt = Operation {
            parent: None,
            before: id,
            after: id,
            kind: OperationKind::Adopt,
            timestamp_seconds: 0,
        };
        let mut unknown = adopt.encode().unwrap();
        let kind_byte = unknown.len() - 1;
        unknown[kind_byte] = 7;
        assert!(Operation::decode(&unknown).is_err());
    }

    #[test]
    fn policy_records_round_trip_within_bounds() {
        let record = policy_record(CapturePolicy::default());
        assert_eq!(parse_policy(&record).unwrap(), CapturePolicy::default());

        let mut tampered = record;
        *tampered.last_mut().unwrap() = 1;
        assert!(parse_policy(&tampered).is_err());

        assert!(CapturePolicy { max_file_bytes: 0 }.validate().is_ok());
        assert!(
            CapturePolicy {
                max_file_bytes: CapturePolicy::MAX_FILE_BYTES
            }
            .validate()
            .is_ok()
        );
        assert!(
            CapturePolicy {
                max_file_bytes: CapturePolicy::MAX_FILE_BYTES + 1
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn initialize_is_exclusive_and_preserves_legacy_stores() {
        let git_dir = tempfile::tempdir().unwrap();
        let root = git_dir.path().join("git2");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("format"), b"1\n").unwrap();
        assert!(RecoveryStore::initialize(git_dir.path(), CapturePolicy::default()).is_err());
        assert!(matches!(
            RecoveryStore::open(git_dir.path()),
            Err(Error::UnsupportedRepository(message)) if message.contains('1')
        ));
        assert_eq!(std::fs::read(root.join("format")).unwrap(), b"1\n");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn initialize_refuses_oversize_policy_before_touching_disk() {
        let git_dir = tempfile::tempdir().unwrap();
        assert!(
            RecoveryStore::initialize(
                git_dir.path(),
                CapturePolicy {
                    max_file_bytes: CapturePolicy::MAX_FILE_BYTES + 1
                },
            )
            .is_err()
        );
        assert!(!git_dir.path().join("git2").exists());
    }

    #[test]
    fn exists_reports_store_presence_without_creating_it() {
        let git_dir = tempfile::tempdir().unwrap();
        assert!(!RecoveryStore::exists(git_dir.path()).unwrap());
        RecoveryStore::initialize(git_dir.path(), CapturePolicy::default()).unwrap();
        assert!(RecoveryStore::exists(git_dir.path()).unwrap());
        assert_eq!(
            std::fs::read_dir(git_dir.path().join("git2"))
                .unwrap()
                .count(),
            6
        );
    }

    #[test]
    fn set_head_requires_the_referenced_views() {
        let git_dir = tempfile::tempdir().unwrap();
        let store = RecoveryStore::initialize(git_dir.path(), CapturePolicy::default()).unwrap();
        let missing = ContentId::for_bytes(b"missing view");
        let operation_id = store
            .put_operation(&Operation {
                parent: None,
                before: missing,
                after: missing,
                kind: OperationKind::Adopt,
                timestamp_seconds: 0,
            })
            .unwrap();
        assert!(store.set_head(operation_id).is_err());
        assert!(store.head().is_err());
    }
}
