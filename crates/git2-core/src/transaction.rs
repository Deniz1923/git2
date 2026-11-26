//! Constrained 4-phase mutation transaction journal and crash recovery.
//!
//! Every mutating `git2` operation records a durable pre-operation view and an
//! immutable action plan under `.git/git2/pending/` before touching ordinary Git
//! state. Metadata locks are staged inside `pending/` first and hard-linked
//! without replacement so recovery can prove ownership by `(st_dev, st_ino)` and
//! never modifies an unknown lock.

use std::ffi::OsStr;
use std::fs;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use bstr::{BString, ByteSlice};
use rustix::fs::{AtFlags, fsync, renameat, unlinkat};

use crate::object::{GitOid, parse_canonical_object};
use crate::paths::{CheckedRoot, PathEntryKind, checked_split};
use crate::recovery::{
    CapturePolicy, ContentId, DIGEST_LEN, FileKind, MAX_CAS_BYTES, MAX_OPERATION_BYTES,
    MAX_VIEW_BYTES, Operation, OperationKind, RecoveryStore, add_sizes, check_count, checked_field,
    read_bytes, read_content_id, read_count, read_fixed, read_u64, take_byte, take_exact,
    write_bytes, write_count,
};
use crate::repository::{LocalRepository, current_timestamp_seconds};
use crate::retention::MAX_OBJECT_BYTES;
use crate::store::ObjectStore;
use crate::{Error, Result};

const PLAN_MAGIC: &[u8] = b"git2-plan\0\x01";
const PENDING_MAGIC: &[u8] = b"git2-pending\0\x01";
const ACTIVE_RECORD: &str = "active";
const PLAN_RECORD: &str = "plan";
const OID_LEN: usize = 20;

/// A validated sidecar and repository anchor that does not require a valid index or complete worktree.
#[derive(Debug, Clone)]
pub struct RecoveryRepository {
    git_dir: PathBuf,
    workdir: PathBuf,
    store: RecoveryStore,
}

impl RecoveryRepository {
    /// Open an adopted repository by walking upward from `path` to its real `.git` directory.
    ///
    /// Unlike [`LocalRepository::discover`], this succeeds even if an interrupted mutation left
    /// the index or working tree incomplete.
    ///
    /// # Errors
    ///
    /// Returns an error if no real `.git` directory is found or the private recovery store is
    /// absent, unsafe, legacy, or corrupt.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let start = fs::canonicalize(path.as_ref())?;
        for candidate_workdir in start.ancestors() {
            let candidate_git = candidate_workdir.join(".git");
            let Ok(metadata) = fs::symlink_metadata(&candidate_git) else {
                continue;
            };
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(Error::UnsupportedRepository(
                    "Git metadata must live in the worktree's own `.git` directory".to_owned(),
                ));
            }
            let store = RecoveryStore::open(&candidate_git)?;
            return Ok(Self {
                git_dir: candidate_git,
                workdir: candidate_workdir.to_path_buf(),
                store,
            });
        }
        Err(Error::UnsupportedRepository(
            "could not find a supported Git repository".to_owned(),
        ))
    }

    /// Return the `.git` metadata directory.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Return the working-tree root directory.
    #[must_use]
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Return the open private recovery store.
    #[must_use]
    pub fn store(&self) -> &RecoveryStore {
        &self.store
    }
}

/// Expected or target state of one captured worktree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryState {
    pub kind: FileKind,
    pub content: ContentId,
}

/// One primitive checked action in the constrained mutation plan vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    RehydrateObject {
        oid: GitOid,
        content: ContentId,
    },
    WriteWorktreeEntry {
        path: BString,
        expected_before: Option<EntryState>,
        target: EntryState,
    },
    RemoveWorktreeEntry {
        path: BString,
        expected_before: EntryState,
    },
    EnsureDir {
        path: BString,
    },
    RemoveEmptyDir {
        path: BString,
    },
    WriteIndex {
        expected_before: Option<ContentId>,
        target: ContentId,
    },
    RemoveIndex {
        expected_before: ContentId,
    },
    WriteRef {
        name: BString,
        expected_before: Option<BString>,
        target: BString,
    },
    RemoveRef {
        name: BString,
        expected_before: BString,
    },
    WritePackedRefs {
        expected_before: Option<BString>,
        target: Option<BString>,
    },
    WriteHead {
        expected_before: BString,
        target: BString,
    },
}

/// A self-contained mutation plan bound to observed recovery and live-repository views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationPlan {
    pub expected_head: ContentId,
    pub before_view: ContentId,
    pub target_view: ContentId,
    pub command: BString,
    pub actions: Vec<Action>,
}

/// The four persisted phases of a pending transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingPhase {
    Prepared = 1,
    Applying = 2,
    Finalizing = 3,
    Committed = 4,
}

impl PendingPhase {
    const fn code(self) -> u8 {
        self as u8
    }

    fn from_code(code: u8) -> Result<Self> {
        match code {
            1 => Ok(Self::Prepared),
            2 => Ok(Self::Applying),
            3 => Ok(Self::Finalizing),
            4 => Ok(Self::Committed),
            _ => Err(Error::CorruptRecovery(format!(
                "unknown pending transaction phase {code}"
            ))),
        }
    }
}

/// Read-only diagnostic summary of a pending transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSummary {
    pub tx_id: ContentId,
    pub parent: ContentId,
    pub before_view: ContentId,
    pub target_view: ContentId,
    pub plan_id: ContentId,
    pub command: BString,
    pub phase: PendingPhase,
    pub next_action: usize,
    pub total_actions: usize,
    pub operation_id: ContentId,
}

/// Result of reconciling or rolling back a pending transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    Clean,
    Aborted {
        before_view: ContentId,
    },
    RolledBack {
        before_view: ContentId,
    },
    Finalized {
        operation_id: ContentId,
        after_view: ContentId,
    },
}

/// Deterministic failure-injection hooks used by crash-recovery tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failpoint {
    BeforePendingWrite,
    AfterPrepare,
    BeforeLockPublish(usize),
    AfterLockPublish(usize),
    BeforeAction(usize),
    AfterAction(usize),
    BeforeFinalize,
    AfterOperationWrite,
    AfterHeadSelection,
    BeforePendingCleanup,
    RecoverAfterOperationWrite,
    RecoverAfterHeadSelection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LockOwner {
    action_index: usize,
    dev: u64,
    ino: u64,
    lock_rel_path: BString,
    stage_name: BString,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRecord {
    tx_id: ContentId,
    parent: ContentId,
    before_view: ContentId,
    target_view: ContentId,
    plan_id: ContentId,
    timestamp_seconds: i64,
    command: BString,
    phase: PendingPhase,
    next_action: usize,
    lock_owner: Option<LockOwner>,
}

impl PendingRecord {
    fn operation(&self) -> Operation {
        Operation {
            parent: Some(self.parent),
            before: self.before_view,
            after: self.target_view,
            kind: OperationKind::Git2(self.command.clone()),
            timestamp_seconds: self.timestamp_seconds,
        }
    }

    fn operation_id(&self) -> Result<ContentId> {
        Ok(ContentId::for_bytes(&self.operation().encode()?))
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut total = PENDING_MAGIC.len()
            + DIGEST_LEN * 5
            + size_of::<i64>()
            + size_of::<u64>()
            + checked_field(self.command.len(), "pending command")?
            + 1
            + size_of::<u64>()
            + 1;
        if let Some(owner) = &self.lock_owner {
            total = add_sizes(total, size_of::<u64>() * 3)?;
            total = add_sizes(total, size_of::<u64>())?;
            total = add_sizes(
                total,
                checked_field(owner.lock_rel_path.len(), "lock path")?,
            )?;
            total = add_sizes(total, size_of::<u64>())?;
            total = add_sizes(total, checked_field(owner.stage_name.len(), "stage name")?)?;
        }
        if total > MAX_OPERATION_BYTES {
            return Err(Error::CorruptRecovery(format!(
                "pending record is {total} bytes, above the {MAX_OPERATION_BYTES} byte limit"
            )));
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(PENDING_MAGIC);
        out.extend_from_slice(self.tx_id.as_bytes());
        out.extend_from_slice(self.parent.as_bytes());
        out.extend_from_slice(self.before_view.as_bytes());
        out.extend_from_slice(self.target_view.as_bytes());
        out.extend_from_slice(self.plan_id.as_bytes());
        out.extend_from_slice(&self.timestamp_seconds.to_le_bytes());
        write_bytes(&mut out, self.command.as_ref());
        out.push(self.phase.code());
        write_count(&mut out, self.next_action);
        match &self.lock_owner {
            Some(owner) => {
                out.push(1);
                write_count(&mut out, owner.action_index);
                out.extend_from_slice(&owner.dev.to_le_bytes());
                out.extend_from_slice(&owner.ino.to_le_bytes());
                write_bytes(&mut out, owner.lock_rel_path.as_ref());
                write_bytes(&mut out, owner.stage_name.as_ref());
            }
            None => out.push(0),
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_OPERATION_BYTES {
            return Err(Error::CorruptRecovery(format!(
                "pending record of {} bytes exceeds the {MAX_OPERATION_BYTES} byte limit",
                bytes.len()
            )));
        }
        let mut input = bytes;
        take_exact(&mut input, PENDING_MAGIC.len())
            .filter(|magic| *magic == PENDING_MAGIC)
            .ok_or_else(|| Error::CorruptRecovery("invalid pending record header".to_owned()))?;
        let tx_id = read_content_id(&mut input, "transaction id")?;
        let parent = read_content_id(&mut input, "parent operation")?;
        let before_view = read_content_id(&mut input, "before view")?;
        let target_view = read_content_id(&mut input, "target view")?;
        let plan_id = read_content_id(&mut input, "plan id")?;
        let ts_bytes = read_fixed::<8>(&mut input, "pending timestamp")?;
        let timestamp_seconds = i64::from_le_bytes(ts_bytes);
        let command = read_bytes(&mut input)?;
        let phase = PendingPhase::from_code(take_byte(&mut input)?)?;
        let next_action = read_count(&mut input)?;
        let lock_owner = match take_byte(&mut input)? {
            0 => None,
            1 => {
                let action_index = read_count(&mut input)?;
                let dev = read_u64(&mut input)?;
                let ino = read_u64(&mut input)?;
                let lock_rel_path = read_bytes(&mut input)?;
                let stage_name = read_bytes(&mut input)?;
                Some(LockOwner {
                    action_index,
                    dev,
                    ino,
                    lock_rel_path,
                    stage_name,
                })
            }
            marker => {
                return Err(Error::CorruptRecovery(format!(
                    "invalid pending lock-owner marker {marker}"
                )));
            }
        };
        if !input.is_empty() {
            return Err(Error::CorruptRecovery(
                "pending record has trailing bytes".to_owned(),
            ));
        }
        Ok(Self {
            tx_id,
            parent,
            before_view,
            target_view,
            plan_id,
            timestamp_seconds,
            command,
            phase,
            next_action,
            lock_owner,
        })
    }
}

fn encode_plan(actions: &[Action]) -> Result<Vec<u8>> {
    let mut total = PLAN_MAGIC.len() + size_of::<u64>();
    for action in actions {
        total = add_sizes(total, encoded_action_size(action)?)?;
    }
    if total > MAX_VIEW_BYTES {
        return Err(Error::CorruptRecovery(format!(
            "the encoded plan is {total} bytes, above the {MAX_VIEW_BYTES} byte limit"
        )));
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(PLAN_MAGIC);
    write_count(&mut out, actions.len());
    for action in actions {
        encode_action(&mut out, action);
    }
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

fn encoded_action_size(action: &Action) -> Result<usize> {
    let mut size = 1_usize;
    match action {
        Action::RehydrateObject { .. } => {
            size = add_sizes(size, OID_LEN + DIGEST_LEN)?;
        }
        Action::WriteWorktreeEntry {
            path,
            expected_before,
            ..
        } => {
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(path.len(), "action path")?)?;
            size = add_sizes(
                size,
                1 + if expected_before.is_some() {
                    1 + DIGEST_LEN
                } else {
                    0
                },
            )?;
            size = add_sizes(size, 1 + DIGEST_LEN)?;
        }
        Action::RemoveWorktreeEntry { path, .. } => {
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(path.len(), "action path")?)?;
            size = add_sizes(size, 1 + DIGEST_LEN)?;
        }
        Action::EnsureDir { path } | Action::RemoveEmptyDir { path } => {
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(path.len(), "action dir")?)?;
        }
        Action::WriteIndex {
            expected_before, ..
        } => {
            size = add_sizes(
                size,
                1 + if expected_before.is_some() {
                    DIGEST_LEN
                } else {
                    0
                },
            )?;
            size = add_sizes(size, DIGEST_LEN)?;
        }
        Action::RemoveIndex { .. } => {
            size = add_sizes(size, DIGEST_LEN)?;
        }
        Action::WriteRef {
            name,
            expected_before,
            target,
        } => {
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(name.len(), "ref name")?)?;
            size = add_sizes(size, 1)?;
            if let Some(before) = expected_before {
                size = add_sizes(size, size_of::<u64>())?;
                size = add_sizes(size, checked_field(before.len(), "expected ref")?)?;
            }
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(target.len(), "target ref")?)?;
        }
        Action::RemoveRef {
            name,
            expected_before,
        } => {
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(name.len(), "ref name")?)?;
            size = add_sizes(size, size_of::<u64>())?;
            size = add_sizes(size, checked_field(expected_before.len(), "expected ref")?)?;
        }
        Action::WritePackedRefs {
            expected_before,
            target,
        } => {
            size = add_sizes(size, 2)?;
            if let Some(before) = expected_before {
                size = add_sizes(size, size_of::<u64>())?;
                size = add_sizes(size, checked_field(before.len(), "expected packed-refs")?)?;
            }
            if let Some(after) = target {
                size = add_sizes(size, size_of::<u64>())?;
                size = add_sizes(size, checked_field(after.len(), "target packed-refs")?)?;
            }
        }
        Action::WriteHead {
            expected_before,
            target,
        } => {
            size = add_sizes(size, size_of::<u64>() * 2)?;
            size = add_sizes(size, checked_field(expected_before.len(), "expected HEAD")?)?;
            size = add_sizes(size, checked_field(target.len(), "target HEAD")?)?;
        }
    }
    Ok(size)
}

fn encode_action(out: &mut Vec<u8>, action: &Action) {
    match action {
        Action::RehydrateObject { oid, content } => {
            out.push(1);
            out.extend_from_slice(&oid.as_bytes());
            out.extend_from_slice(content.as_bytes());
        }
        Action::WriteWorktreeEntry {
            path,
            expected_before,
            target,
        } => {
            out.push(2);
            write_bytes(out, path.as_ref());
            match expected_before {
                Some(state) => {
                    out.push(1);
                    out.push(state.kind.code());
                    out.extend_from_slice(state.content.as_bytes());
                }
                None => out.push(0),
            }
            out.push(target.kind.code());
            out.extend_from_slice(target.content.as_bytes());
        }
        Action::RemoveWorktreeEntry {
            path,
            expected_before,
        } => {
            out.push(3);
            write_bytes(out, path.as_ref());
            out.push(expected_before.kind.code());
            out.extend_from_slice(expected_before.content.as_bytes());
        }
        Action::EnsureDir { path } => {
            out.push(4);
            write_bytes(out, path.as_ref());
        }
        Action::RemoveEmptyDir { path } => {
            out.push(5);
            write_bytes(out, path.as_ref());
        }
        Action::WriteIndex {
            expected_before,
            target,
        } => {
            out.push(6);
            match expected_before {
                Some(id) => {
                    out.push(1);
                    out.extend_from_slice(id.as_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(target.as_bytes());
        }
        Action::RemoveIndex { expected_before } => {
            out.push(7);
            out.extend_from_slice(expected_before.as_bytes());
        }
        Action::WriteRef {
            name,
            expected_before,
            target,
        } => {
            out.push(8);
            write_bytes(out, name.as_ref());
            encode_opt_bytes(out, expected_before.as_ref());
            write_bytes(out, target.as_ref());
        }
        Action::RemoveRef {
            name,
            expected_before,
        } => {
            out.push(9);
            write_bytes(out, name.as_ref());
            write_bytes(out, expected_before.as_ref());
        }
        Action::WritePackedRefs {
            expected_before,
            target,
        } => {
            out.push(10);
            encode_opt_bytes(out, expected_before.as_ref());
            encode_opt_bytes(out, target.as_ref());
        }
        Action::WriteHead {
            expected_before,
            target,
        } => {
            out.push(11);
            write_bytes(out, expected_before.as_ref());
            write_bytes(out, target.as_ref());
        }
    }
}

fn encode_opt_bytes(out: &mut Vec<u8>, value: Option<&BString>) {
    match value {
        Some(bytes) => {
            out.push(1);
            write_bytes(out, bytes.as_ref());
        }
        None => out.push(0),
    }
}

fn decode_plan(bytes: &[u8]) -> Result<Vec<Action>> {
    if bytes.len() > MAX_VIEW_BYTES {
        return Err(Error::CorruptRecovery(format!(
            "plan record of {} bytes exceeds the {MAX_VIEW_BYTES} byte limit",
            bytes.len()
        )));
    }
    let mut input = bytes;
    take_exact(&mut input, PLAN_MAGIC.len())
        .filter(|magic| *magic == PLAN_MAGIC)
        .ok_or_else(|| Error::CorruptRecovery("invalid plan record header".to_owned()))?;
    let count = read_count(&mut input)?;
    check_count(count, 2, input.len(), "plan action")?;
    let mut actions = Vec::with_capacity(count);
    for _ in 0..count {
        actions.push(decode_action(&mut input)?);
    }
    if !input.is_empty() {
        return Err(Error::CorruptRecovery(
            "plan record has trailing bytes".to_owned(),
        ));
    }
    Ok(actions)
}

fn decode_action(input: &mut &[u8]) -> Result<Action> {
    match take_byte(input)? {
        1 => Ok(Action::RehydrateObject {
            oid: GitOid::from_bytes(read_fixed::<OID_LEN>(input, "rehydrate oid")?),
            content: read_content_id(input, "rehydrate content")?,
        }),
        2 => {
            let path = read_bytes(input)?;
            let expected_before = match take_byte(input)? {
                0 => None,
                1 => Some(decode_entry_state(input)?),
                marker => {
                    return Err(Error::CorruptRecovery(format!(
                        "invalid expected-before marker {marker}"
                    )));
                }
            };
            let target = decode_entry_state(input)?;
            Ok(Action::WriteWorktreeEntry {
                path,
                expected_before,
                target,
            })
        }
        3 => Ok(Action::RemoveWorktreeEntry {
            path: read_bytes(input)?,
            expected_before: decode_entry_state(input)?,
        }),
        4 => Ok(Action::EnsureDir {
            path: read_bytes(input)?,
        }),
        5 => Ok(Action::RemoveEmptyDir {
            path: read_bytes(input)?,
        }),
        6 => {
            let expected_before = match take_byte(input)? {
                0 => None,
                1 => Some(read_content_id(input, "expected index")?),
                marker => {
                    return Err(Error::CorruptRecovery(format!(
                        "invalid expected-index marker {marker}"
                    )));
                }
            };
            let target = read_content_id(input, "target index")?;
            Ok(Action::WriteIndex {
                expected_before,
                target,
            })
        }
        7 => Ok(Action::RemoveIndex {
            expected_before: read_content_id(input, "expected index")?,
        }),
        8 => Ok(Action::WriteRef {
            name: read_bytes(input)?,
            expected_before: decode_opt_bytes(input, "expected ref")?,
            target: read_bytes(input)?,
        }),
        9 => Ok(Action::RemoveRef {
            name: read_bytes(input)?,
            expected_before: read_bytes(input)?,
        }),
        10 => Ok(Action::WritePackedRefs {
            expected_before: decode_opt_bytes(input, "expected packed-refs")?,
            target: decode_opt_bytes(input, "target packed-refs")?,
        }),
        11 => Ok(Action::WriteHead {
            expected_before: read_bytes(input)?,
            target: read_bytes(input)?,
        }),
        code => Err(Error::CorruptRecovery(format!(
            "unknown transaction action code {code}"
        ))),
    }
}

fn decode_entry_state(input: &mut &[u8]) -> Result<EntryState> {
    let kind = FileKind::from_code(take_byte(input)?)?;
    let content = read_content_id(input, "entry content")?;
    Ok(EntryState { kind, content })
}

fn decode_opt_bytes(input: &mut &[u8], noun: &str) -> Result<Option<BString>> {
    match take_byte(input)? {
        0 => Ok(None),
        1 => Ok(Some(read_bytes(input)?)),
        marker => Err(Error::CorruptRecovery(format!(
            "invalid {noun} marker {marker}"
        ))),
    }
}

/// Inspect any pending transaction without taking the lock or modifying repository state.
///
/// # Errors
///
/// Returns an error if the pending record or plan is unreadable, corrupt, or fails verification.
pub fn pending(repo: &RecoveryRepository) -> Result<Option<PendingSummary>> {
    let dir = repo.store().pending_dir()?;
    if !dir.exists(ACTIVE_RECORD)? {
        return Ok(None);
    }
    let record = PendingRecord::decode(&dir.read(ACTIVE_RECORD, MAX_OPERATION_BYTES)?)?;
    let plan_bytes = dir.read(PLAN_RECORD, MAX_VIEW_BYTES).map_err(|error| {
        if matches!(&error, Error::Io(cause) if cause.kind() == std::io::ErrorKind::NotFound) {
            Error::CorruptRecovery("pending transaction is missing its plan record".to_owned())
        } else {
            error
        }
    })?;
    if ContentId::for_bytes(&plan_bytes) != record.plan_id {
        return Err(Error::CorruptRecovery(
            "pending plan does not match the active pending transaction".to_owned(),
        ));
    }
    let actions = decode_plan(&plan_bytes)?;
    if record.next_action > actions.len() {
        return Err(Error::CorruptRecovery(
            "pending action index exceeds total plan actions".to_owned(),
        ));
    }
    let operation_id = record.operation_id()?;
    Ok(Some(PendingSummary {
        tx_id: record.tx_id,
        parent: record.parent,
        before_view: record.before_view,
        target_view: record.target_view,
        plan_id: record.plan_id,
        command: record.command,
        phase: record.phase,
        next_action: record.next_action,
        total_actions: actions.len(),
        operation_id,
    }))
}

/// Execute a validated mutation plan atomically and record one new recovery operation.
///
/// # Errors
///
/// Returns an error if another writer holds the lock, a pending transaction already exists,
/// the plan is stale, an action or foreign lock conflicts with live state, or durability fails.
pub fn run(repo: &LocalRepository, plan: &MutationPlan) -> Result<ContentId> {
    run_with_hook(repo, plan, |_| Ok(()))
}

#[doc(hidden)]
pub fn run_with_hook(
    repo: &LocalRepository,
    plan: &MutationPlan,
    mut hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<ContentId> {
    let store = RecoveryStore::open(repo.git_dir())?;
    let _lock = store.try_lock()?;
    if store.has_pending()? {
        return Err(Error::UnsupportedRepository(
            "a pending transaction exists; reconcile it with `recover` first".to_owned(),
        ));
    }
    let observed_head = store.head()?;
    if observed_head != plan.expected_head {
        return Err(Error::UnsupportedRepository(format!(
            "stale mutation plan: expected recovery HEAD {}, found {observed_head}",
            plan.expected_head
        )));
    }

    let policy = store.policy()?;
    store.read_view(plan.target_view)?;
    validate_plan_inputs(&store, &plan.actions, policy)?;
    let plan_bytes = encode_plan(&plan.actions)?;
    let plan_id = ContentId::for_bytes(&plan_bytes);

    let live_view = repo.capture_and_persist_view(&store)?;
    if live_view != plan.before_view {
        return Err(Error::UnsupportedRepository(format!(
            "stale mutation plan: expected live view {}, found {live_view}",
            plan.before_view
        )));
    }
    let parent = publish_external_if_needed(&store, observed_head, live_view)?;
    let timestamp_seconds = current_timestamp_seconds();

    let mut pending_input = Vec::new();
    pending_input.extend_from_slice(parent.as_bytes());
    pending_input.extend_from_slice(plan.before_view.as_bytes());
    pending_input.extend_from_slice(plan.target_view.as_bytes());
    pending_input.extend_from_slice(plan_id.as_bytes());
    pending_input.extend_from_slice(&timestamp_seconds.to_le_bytes());
    pending_input.extend_from_slice(plan.command.as_ref());
    let tx_id = ContentId::for_bytes(&pending_input);

    let pending_dir = store.pending_dir()?;
    cleanup_orphan_pending(&store, &pending_dir)?;
    pending_dir.write_pointer(PLAN_RECORD, &plan_bytes)?;

    let mut record = PendingRecord {
        tx_id,
        parent,
        before_view: plan.before_view,
        target_view: plan.target_view,
        plan_id,
        timestamp_seconds,
        command: plan.command.clone(),
        phase: PendingPhase::Prepared,
        next_action: 0,
        lock_owner: None,
    };
    hook(Failpoint::BeforePendingWrite)?;
    pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
    hook(Failpoint::AfterPrepare)?;

    let context = TxContext {
        git_dir: repo.git_dir(),
        workdir: repo.workdir(),
        store: &store,
        policy,
        tx_id,
    };
    apply_actions(&context, &mut record, &plan.actions, &mut hook)?;

    record.phase = PendingPhase::Finalizing;
    record.lock_owner = None;
    pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
    hook(Failpoint::BeforeFinalize)?;

    let actual_after = repo.capture_and_persist_view(&store)?;
    if actual_after != plan.target_view {
        return Err(Error::CorruptRecovery(format!(
            "post-mutation state hashes to view {actual_after}, expected {}",
            plan.target_view
        )));
    }
    let operation = record.operation();
    let operation_id = store.put_operation(&operation)?;
    hook(Failpoint::AfterOperationWrite)?;
    store.set_head(operation_id)?;
    hook(Failpoint::AfterHeadSelection)?;

    record.phase = PendingPhase::Committed;
    pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
    hook(Failpoint::BeforePendingCleanup)?;
    cleanup_pending_state(&context, &plan.actions)?;
    Ok(operation_id)
}

/// Idempotently reconcile a pending transaction after interruption or crash.
///
/// # Errors
///
/// Returns an error if the lock is held, pending metadata is corrupt, an unknown lock blocks
/// progress, or live state diverged from both expected before and after outcomes.
pub fn recover(repo: &RecoveryRepository) -> Result<RecoveryOutcome> {
    recover_with_hook(repo, |_| Ok(()))
}

#[doc(hidden)]
pub fn recover_with_hook(
    repo: &RecoveryRepository,
    mut hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<RecoveryOutcome> {
    let store = repo.store();
    let _lock = store.try_lock()?;
    let pending_dir = store.pending_dir()?;
    if !pending_dir.exists(ACTIVE_RECORD)? {
        cleanup_orphan_pending(store, &pending_dir)?;
        return Ok(RecoveryOutcome::Clean);
    }

    let (mut record, actions) = load_verified_pending(store)?;
    let policy = store.policy()?;
    let context = TxContext {
        git_dir: repo.git_dir(),
        workdir: repo.workdir(),
        store,
        policy,
        tx_id: record.tx_id,
    };
    let expected_op_id = record.operation_id()?;
    let current_head = store.head()?;

    if current_head == expected_op_id {
        if let Some(owner) = &record.lock_owner {
            release_owned_lock(&context, owner)?;
        }
        cleanup_pending_state(&context, &actions)?;
        return Ok(RecoveryOutcome::Finalized {
            operation_id: expected_op_id,
            after_view: record.target_view,
        });
    }
    if current_head != record.parent {
        return Err(Error::CorruptRecovery(format!(
            "recovery HEAD {current_head} matches neither pending parent {} nor expected operation {expected_op_id}",
            record.parent
        )));
    }
    if let Some(owner) = &record.lock_owner {
        verify_lock_if_present(repo.git_dir(), owner)?;
    }

    if record.phase == PendingPhase::Prepared {
        verify_all_at_before(&context, &actions)?;
        cleanup_pending_state(&context, &actions)?;
        return Ok(RecoveryOutcome::Aborted {
            before_view: record.before_view,
        });
    }

    let mut noop = |_| Ok(());
    apply_actions(&context, &mut record, &actions, &mut noop)?;

    record.phase = PendingPhase::Finalizing;
    record.lock_owner = None;
    pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;

    let live_repo = LocalRepository::discover(repo.workdir())?;
    let actual_after = live_repo.capture_and_persist_view(store)?;
    if actual_after != record.target_view {
        return Err(Error::CorruptRecovery(format!(
            "recovered state hashes to view {actual_after}, expected {}",
            record.target_view
        )));
    }
    let operation_id = store.put_operation(&record.operation())?;
    hook(Failpoint::RecoverAfterOperationWrite)?;
    store.set_head(operation_id)?;
    hook(Failpoint::RecoverAfterHeadSelection)?;

    record.phase = PendingPhase::Committed;
    pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
    cleanup_pending_state(&context, &actions)?;
    Ok(RecoveryOutcome::Finalized {
        operation_id,
        after_view: record.target_view,
    })
}

/// Roll back an un-selected pending transaction back to its durable `before_view`.
///
/// # Errors
///
/// Returns an error if the transaction already moved recovery `HEAD`, a foreign lock exists,
/// live state diverged from both expected outcomes, or restoration fails.
pub fn rollback(repo: &RecoveryRepository) -> Result<RecoveryOutcome> {
    let store = repo.store();
    let _lock = store.try_lock()?;
    let pending_dir = store.pending_dir()?;
    if !pending_dir.exists(ACTIVE_RECORD)? {
        cleanup_orphan_pending(store, &pending_dir)?;
        return Ok(RecoveryOutcome::Clean);
    }

    let (record, actions) = load_verified_pending(store)?;
    let policy = store.policy()?;
    let context = TxContext {
        git_dir: repo.git_dir(),
        workdir: repo.workdir(),
        store,
        policy,
        tx_id: record.tx_id,
    };
    let expected_op_id = record.operation_id()?;
    let current_head = store.head()?;
    if current_head == expected_op_id {
        return Err(Error::UnsupportedRepository(
            "pending transaction already selected recovery HEAD; run `recover` instead".to_owned(),
        ));
    }
    if current_head != record.parent {
        return Err(Error::CorruptRecovery(format!(
            "recovery HEAD {current_head} does not match pending parent {}",
            record.parent
        )));
    }
    if let Some(owner) = &record.lock_owner {
        release_owned_lock(&context, owner)?;
    }
    cleanup_worktree_temps(&context, &actions)?;

    for (idx, action) in actions.iter().enumerate().rev() {
        rollback_action(&context, idx, action)?;
    }

    let live_repo = LocalRepository::discover(repo.workdir())?;
    let actual_before = live_repo.capture_and_persist_view(store)?;
    if actual_before != record.before_view {
        return Err(Error::CorruptRecovery(format!(
            "rolled-back state hashes to view {actual_before}, expected {}",
            record.before_view
        )));
    }
    cleanup_pending_state(&context, &actions)?;
    Ok(RecoveryOutcome::RolledBack {
        before_view: record.before_view,
    })
}

struct TxContext<'a> {
    git_dir: &'a Path,
    workdir: &'a Path,
    store: &'a RecoveryStore,
    policy: CapturePolicy,
    tx_id: ContentId,
}

fn publish_external_if_needed(
    store: &RecoveryStore,
    observed_head: ContentId,
    live_view: ContentId,
) -> Result<ContentId> {
    let current_view = store.current_view()?;
    if live_view == current_view {
        return Ok(observed_head);
    }
    let ext = Operation {
        parent: Some(observed_head),
        before: current_view,
        after: live_view,
        kind: OperationKind::ExternalGit,
        timestamp_seconds: current_timestamp_seconds(),
    };
    let ext_id = store.put_operation(&ext)?;
    store.set_head(ext_id)?;
    Ok(ext_id)
}

fn validate_plan_inputs(
    store: &RecoveryStore,
    actions: &[Action],
    policy: CapturePolicy,
) -> Result<()> {
    let mut worktree_limit = usize::try_from(policy.max_file_bytes).unwrap_or(usize::MAX);
    worktree_limit = worktree_limit.min(MAX_CAS_BYTES);
    for action in actions {
        match action {
            Action::RehydrateObject { oid, content } => {
                let canonical = store.read_content_bounded(*content, MAX_OBJECT_BYTES + 32)?;
                let object = parse_canonical_object(&canonical)?;
                if object.id() != *oid {
                    return Err(Error::CorruptRecovery(format!(
                        "retained object {content} hashes to {}, expected {oid}",
                        object.id()
                    )));
                }
            }
            Action::WriteWorktreeEntry {
                path,
                expected_before,
                target,
            } => {
                checked_split(path.as_slice())?;
                if let Some(before) = expected_before {
                    validate_entry_content(store, *before, worktree_limit)?;
                }
                validate_entry_content(store, *target, worktree_limit)?;
            }
            Action::RemoveWorktreeEntry {
                path,
                expected_before,
            } => {
                checked_split(path.as_slice())?;
                validate_entry_content(store, *expected_before, worktree_limit)?;
            }
            Action::EnsureDir { path } | Action::RemoveEmptyDir { path } => {
                checked_split(path.as_slice())?;
            }
            Action::WriteIndex {
                expected_before,
                target,
            } => {
                if let Some(before) = expected_before {
                    store.read_content(*before)?;
                }
                store.read_content(*target)?;
            }
            Action::RemoveIndex { expected_before } => {
                store.read_content(*expected_before)?;
            }
            Action::WriteRef { name, .. } | Action::RemoveRef { name, .. } => {
                validate_ref_name(name.as_slice())?;
            }
            Action::WritePackedRefs { .. } | Action::WriteHead { .. } => {}
        }
    }
    Ok(())
}

fn validate_entry_content(store: &RecoveryStore, entry: EntryState, limit: usize) -> Result<()> {
    let bytes = store.read_content_bounded(entry.content, limit)?;
    if entry.kind == FileKind::Symlink && bytes.len() > 64 * 1024 {
        return Err(Error::UnsupportedRepository(
            "symlink target exceeds maximum supported length".to_owned(),
        ));
    }
    Ok(())
}

fn validate_ref_name(name: &[u8]) -> Result<()> {
    checked_split(name)?;
    if !name.starts_with(b"refs/") {
        return Err(Error::UnsupportedRepository(format!(
            "ref {:?} must start with refs/",
            BString::from(name)
        )));
    }
    gix::validate::reference::name(name.as_bstr())
        .map_err(|error| Error::UnsupportedRepository(format!("invalid ref name: {error}")))?;
    Ok(())
}

fn load_verified_pending(store: &RecoveryStore) -> Result<(PendingRecord, Vec<Action>)> {
    let dir = store.pending_dir()?;
    let record = PendingRecord::decode(&dir.read(ACTIVE_RECORD, MAX_OPERATION_BYTES)?)?;
    let plan_bytes = dir.read(PLAN_RECORD, MAX_VIEW_BYTES).map_err(|error| {
        if matches!(&error, Error::Io(cause) if cause.kind() == std::io::ErrorKind::NotFound) {
            Error::CorruptRecovery("pending transaction is missing its plan record".to_owned())
        } else {
            error
        }
    })?;
    if ContentId::for_bytes(&plan_bytes) != record.plan_id {
        return Err(Error::CorruptRecovery(
            "pending plan does not match the active pending transaction".to_owned(),
        ));
    }
    let actions = decode_plan(&plan_bytes)?;
    if record.next_action > actions.len() {
        return Err(Error::CorruptRecovery(
            "pending action index exceeds total plan actions".to_owned(),
        ));
    }
    store.read_operation(record.parent)?;
    store.read_view(record.before_view)?;
    if record.target_view != record.before_view {
        store.read_view(record.target_view)?;
    }
    Ok((record, actions))
}

fn apply_actions(
    ctx: &TxContext<'_>,
    record: &mut PendingRecord,
    actions: &[Action],
    hook: &mut impl FnMut(Failpoint) -> Result<()>,
) -> Result<()> {
    let pending_dir = ctx.store.pending_dir()?;
    let start = if record
        .lock_owner
        .as_ref()
        .is_some_and(|o| o.action_index < record.next_action)
    {
        return Err(Error::CorruptRecovery(
            "pending lock owner precedes next action cursor".to_owned(),
        ));
    } else {
        record.next_action
    };

    for (idx, action) in actions.iter().enumerate().skip(start) {
        if let Some(meta) = metadata_spec(ctx.store, action)? {
            apply_locked_metadata(ctx, record, &pending_dir, idx, &meta, hook)?;
        } else {
            record.phase = PendingPhase::Applying;
            record.next_action = idx;
            record.lock_owner = None;
            pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
            hook(Failpoint::BeforeAction(idx))?;
            apply_unlocked_action(ctx, idx, action)?;
        }
        record.phase = PendingPhase::Applying;
        record.next_action = idx + 1;
        record.lock_owner = None;
        pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
        hook(Failpoint::AfterAction(idx))?;
    }
    Ok(())
}

struct MetadataActionSpec {
    rel_path: BString,
    lock_rel_path: BString,
    create_parents: bool,
    expected_before: Option<Vec<u8>>,
    target: Option<Vec<u8>>,
}

fn metadata_spec(store: &RecoveryStore, action: &Action) -> Result<Option<MetadataActionSpec>> {
    let spec = match action {
        Action::WriteIndex {
            expected_before,
            target,
        } => MetadataActionSpec {
            rel_path: BString::from(b"index".to_vec()),
            lock_rel_path: BString::from(b"index.lock".to_vec()),
            create_parents: false,
            expected_before: expected_before
                .map(|id| store.read_content(id))
                .transpose()?,
            target: Some(store.read_content(*target)?),
        },
        Action::RemoveIndex { expected_before } => MetadataActionSpec {
            rel_path: BString::from(b"index".to_vec()),
            lock_rel_path: BString::from(b"index.lock".to_vec()),
            create_parents: false,
            expected_before: Some(store.read_content(*expected_before)?),
            target: None,
        },
        Action::WriteRef {
            name,
            expected_before,
            target,
        } => {
            validate_ref_name(name.as_slice())?;
            let mut lock_rel = name.clone();
            lock_rel.extend_from_slice(b".lock");
            MetadataActionSpec {
                rel_path: name.clone(),
                lock_rel_path: lock_rel,
                create_parents: true,
                expected_before: expected_before.as_ref().map(|b| b.to_vec()),
                target: Some(target.to_vec()),
            }
        }
        Action::RemoveRef {
            name,
            expected_before,
        } => {
            validate_ref_name(name.as_slice())?;
            let mut lock_rel = name.clone();
            lock_rel.extend_from_slice(b".lock");
            MetadataActionSpec {
                rel_path: name.clone(),
                lock_rel_path: lock_rel,
                create_parents: false,
                expected_before: Some(expected_before.to_vec()),
                target: None,
            }
        }
        Action::WritePackedRefs {
            expected_before,
            target,
        } => MetadataActionSpec {
            rel_path: BString::from(b"packed-refs".to_vec()),
            lock_rel_path: BString::from(b"packed-refs.lock".to_vec()),
            create_parents: false,
            expected_before: expected_before.as_ref().map(|b| b.to_vec()),
            target: target.as_ref().map(|b| b.to_vec()),
        },
        Action::WriteHead {
            expected_before,
            target,
        } => MetadataActionSpec {
            rel_path: BString::from(b"HEAD".to_vec()),
            lock_rel_path: BString::from(b"HEAD.lock".to_vec()),
            create_parents: false,
            expected_before: Some(expected_before.to_vec()),
            target: Some(target.to_vec()),
        },
        _ => return Ok(None),
    };
    Ok(Some(spec))
}

fn apply_locked_metadata(
    ctx: &TxContext<'_>,
    record: &mut PendingRecord,
    pending_dir: &crate::recovery::disk::Directory,
    idx: usize,
    spec: &MetadataActionSpec,
    hook: &mut impl FnMut(Failpoint) -> Result<()>,
) -> Result<()> {
    let git_meta = CheckedRoot::open(ctx.git_dir)?;
    let actual = git_meta.read_file_raw(spec.rel_path.as_slice(), MAX_CAS_BYTES as u64)?;
    let resumed_owner = record
        .lock_owner
        .clone()
        .filter(|owner| owner.action_index == idx);

    if let Some(owner) = resumed_owner {
        if owner.lock_rel_path != spec.lock_rel_path {
            return Err(Error::CorruptRecovery(
                "pending lock owner path disagrees with plan action".to_owned(),
            ));
        }
        let stage_name = stage_name_str(&owner.stage_name)?;
        if let Some((dev, ino)) = git_meta.stat_regular_dev_ino(owner.lock_rel_path.as_slice())? {
            if (dev, ino) != (owner.dev, owner.ino) {
                return Err(foreign_lock_error(owner.lock_rel_path.as_slice()));
            }
            if actual == spec.target {
                let (parent, lock_leaf) =
                    git_meta.open_parent_dir(owner.lock_rel_path.as_slice(), false)?;
                let _ = unlinkat(
                    parent.as_fd(),
                    OsStr::from_bytes(&lock_leaf),
                    AtFlags::empty(),
                );
                fsync(parent.as_fd()).map_err(std::io::Error::from)?;
                pending_dir.remove_if_exists(stage_name)?;
                return Ok(());
            }
            if actual != spec.expected_before {
                return Err(divergence_error(spec.rel_path.as_slice()));
            }
            commit_locked_metadata(ctx.git_dir, spec)?;
            pending_dir.remove_if_exists(stage_name)?;
            return Ok(());
        }
    }

    if actual == spec.target {
        if let Some(owner) = &record.lock_owner
            && owner.action_index == idx
        {
            pending_dir.remove_if_exists(stage_name_str(&owner.stage_name)?)?;
        }
        return Ok(());
    }
    if actual != spec.expected_before {
        return Err(divergence_error(spec.rel_path.as_slice()));
    }

    let stage_name = stage_file_name(ctx.tx_id, idx);
    let stage_bytes = spec.target.as_deref().unwrap_or(&[]);
    let (dev, ino) = pending_dir.create_stage_file(&stage_name, stage_bytes)?;
    record.phase = PendingPhase::Applying;
    record.next_action = idx;
    record.lock_owner = Some(LockOwner {
        action_index: idx,
        dev,
        ino,
        lock_rel_path: spec.lock_rel_path.clone(),
        stage_name: BString::from(stage_name.as_str()),
    });
    pending_dir.write_pointer(ACTIVE_RECORD, &record.encode()?)?;
    hook(Failpoint::BeforeLockPublish(idx))?;

    let (parent, lock_leaf) =
        git_meta.open_parent_dir(spec.lock_rel_path.as_slice(), spec.create_parents)?;
    let linked = pending_dir.link_to(&stage_name, parent.as_fd(), OsStr::from_bytes(&lock_leaf))?;
    if !linked {
        let existing = git_meta.stat_regular_dev_ino(spec.lock_rel_path.as_slice())?;
        if existing != Some((dev, ino)) {
            let _ = pending_dir.remove_if_exists(&stage_name);
            return Err(foreign_lock_error(spec.lock_rel_path.as_slice()));
        }
    }
    fsync(parent.as_fd()).map_err(std::io::Error::from)?;
    hook(Failpoint::AfterLockPublish(idx))?;

    let rechecked = git_meta.read_file_raw(spec.rel_path.as_slice(), MAX_CAS_BYTES as u64)?;
    if rechecked != spec.expected_before {
        if git_meta.stat_regular_dev_ino(spec.lock_rel_path.as_slice())? == Some((dev, ino)) {
            let _ = unlinkat(
                parent.as_fd(),
                OsStr::from_bytes(&lock_leaf),
                AtFlags::empty(),
            );
            let _ = fsync(parent.as_fd());
        }
        let _ = pending_dir.remove_if_exists(&stage_name);
        return Err(divergence_error(spec.rel_path.as_slice()));
    }

    commit_locked_metadata(ctx.git_dir, spec)?;
    pending_dir.remove_if_exists(&stage_name)?;
    Ok(())
}

fn commit_locked_metadata(git_dir: &Path, spec: &MetadataActionSpec) -> Result<()> {
    let git_meta = CheckedRoot::open(git_dir)?;
    let (parent, target_leaf) =
        git_meta.open_parent_dir(spec.rel_path.as_slice(), spec.create_parents)?;
    let (_, lock_leaf) = git_meta.open_parent_dir(spec.lock_rel_path.as_slice(), false)?;
    let target_os = OsStr::from_bytes(&target_leaf);
    let lock_os = OsStr::from_bytes(&lock_leaf);
    if spec.target.is_some() {
        renameat(parent.as_fd(), lock_os, parent.as_fd(), target_os)
            .map_err(std::io::Error::from)?;
        fsync(parent.as_fd()).map_err(std::io::Error::from)?;
    } else {
        match unlinkat(parent.as_fd(), target_os, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        fsync(parent.as_fd()).map_err(std::io::Error::from)?;
        match unlinkat(parent.as_fd(), lock_os, AtFlags::empty()) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        fsync(parent.as_fd()).map_err(std::io::Error::from)?;
    }
    Ok(())
}

fn apply_unlocked_action(ctx: &TxContext<'_>, idx: usize, action: &Action) -> Result<()> {
    match action {
        Action::RehydrateObject { oid, content } => {
            let objects = ObjectStore::open(ctx.git_dir)?;
            match objects.read(*oid) {
                Ok(existing) if existing.id() == *oid => Ok(()),
                Ok(_) => Err(Error::CorruptObject(format!(
                    "existing Git object {oid} failed digest verification"
                ))),
                Err(Error::ObjectNotFound(_)) => {
                    let canonical = ctx
                        .store
                        .read_content_bounded(*content, MAX_OBJECT_BYTES + 32)?;
                    let object = parse_canonical_object(&canonical)?;
                    if object.id() != *oid {
                        return Err(Error::CorruptRecovery(format!(
                            "retained object {content} hashes to {}, expected {oid}",
                            object.id()
                        )));
                    }
                    objects.write(&object)?;
                    Ok(())
                }
                Err(error) => Err(error),
            }
        }
        Action::WriteWorktreeEntry {
            path,
            expected_before,
            target,
        } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            let actual = read_entry_state(&root, path.as_slice(), ctx.policy.max_file_bytes)?;
            if actual == Some(*target) {
                return Ok(());
            }
            if actual != *expected_before {
                return Err(divergence_error(path.as_slice()));
            }
            let bytes = ctx.store.read_content_bounded(
                target.content,
                usize::try_from(ctx.policy.max_file_bytes).unwrap_or(usize::MAX),
            )?;
            let temp_name = worktree_temp_name(ctx.tx_id, idx);
            root.write_entry_atomic(path.as_slice(), target.kind, &bytes, &temp_name)
        }
        Action::RemoveWorktreeEntry {
            path,
            expected_before,
        } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            let actual = read_entry_state(&root, path.as_slice(), ctx.policy.max_file_bytes)?;
            if actual.is_none() {
                return Ok(());
            }
            if actual != Some(*expected_before) {
                return Err(divergence_error(path.as_slice()));
            }
            root.remove_entry(path.as_slice())
        }
        Action::EnsureDir { path } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            match root.entry_kind(path.as_slice())? {
                Some(PathEntryKind::Directory) => Ok(()),
                None => root.ensure_dir(path.as_slice()),
                Some(_) => Err(divergence_error(path.as_slice())),
            }
        }
        Action::RemoveEmptyDir { path } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            match root.entry_kind(path.as_slice())? {
                None => Ok(()),
                Some(PathEntryKind::Directory) => root.remove_empty_dir(path.as_slice()),
                Some(_) => Err(divergence_error(path.as_slice())),
            }
        }
        Action::WriteIndex { .. }
        | Action::RemoveIndex { .. }
        | Action::WriteRef { .. }
        | Action::RemoveRef { .. }
        | Action::WritePackedRefs { .. }
        | Action::WriteHead { .. } => unreachable!("handled by locked metadata path"),
    }
}

fn rollback_action(ctx: &TxContext<'_>, idx: usize, action: &Action) -> Result<()> {
    match action {
        Action::RehydrateObject { .. } => Ok(()),
        Action::WriteWorktreeEntry {
            path,
            expected_before,
            target,
        } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            let actual = read_entry_state(&root, path.as_slice(), ctx.policy.max_file_bytes)?;
            if actual == *expected_before {
                return Ok(());
            }
            if actual != Some(*target) {
                return Err(divergence_error(path.as_slice()));
            }
            match expected_before {
                Some(before) => {
                    let bytes = ctx.store.read_content_bounded(
                        before.content,
                        usize::try_from(ctx.policy.max_file_bytes).unwrap_or(usize::MAX),
                    )?;
                    let temp_name = worktree_temp_name(ctx.tx_id, idx);
                    root.write_entry_atomic(path.as_slice(), before.kind, &bytes, &temp_name)
                }
                None => root.remove_entry(path.as_slice()),
            }
        }
        Action::RemoveWorktreeEntry {
            path,
            expected_before,
        } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            let actual = read_entry_state(&root, path.as_slice(), ctx.policy.max_file_bytes)?;
            if actual == Some(*expected_before) {
                return Ok(());
            }
            if actual.is_some() {
                return Err(divergence_error(path.as_slice()));
            }
            let bytes = ctx.store.read_content_bounded(
                expected_before.content,
                usize::try_from(ctx.policy.max_file_bytes).unwrap_or(usize::MAX),
            )?;
            let temp_name = worktree_temp_name(ctx.tx_id, idx);
            root.write_entry_atomic(path.as_slice(), expected_before.kind, &bytes, &temp_name)
        }
        Action::EnsureDir { path } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            match root.entry_kind(path.as_slice())? {
                None => Ok(()),
                Some(PathEntryKind::Directory) => root.remove_empty_dir(path.as_slice()),
                Some(_) => Err(divergence_error(path.as_slice())),
            }
        }
        Action::RemoveEmptyDir { path } => {
            let root = CheckedRoot::open(ctx.workdir)?;
            match root.entry_kind(path.as_slice())? {
                Some(PathEntryKind::Directory) => Ok(()),
                None => root.ensure_dir(path.as_slice()),
                Some(_) => Err(divergence_error(path.as_slice())),
            }
        }
        _ => {
            let spec = metadata_spec(ctx.store, action)?.expect("metadata variant");
            let reversed = MetadataActionSpec {
                rel_path: spec.rel_path,
                lock_rel_path: spec.lock_rel_path,
                create_parents: spec.create_parents,
                expected_before: spec.target,
                target: spec.expected_before,
            };
            rollback_locked_metadata(ctx, idx, &reversed)
        }
    }
}

fn rollback_locked_metadata(
    ctx: &TxContext<'_>,
    idx: usize,
    spec: &MetadataActionSpec,
) -> Result<()> {
    let git_meta = CheckedRoot::open(ctx.git_dir)?;
    let actual = git_meta.read_file_raw(spec.rel_path.as_slice(), MAX_CAS_BYTES as u64)?;
    if actual == spec.target {
        return Ok(());
    }
    if actual != spec.expected_before {
        return Err(divergence_error(spec.rel_path.as_slice()));
    }
    let pending_dir = ctx.store.pending_dir()?;
    let stage_name = stage_file_name(ctx.tx_id, idx);
    let stage_bytes = spec.target.as_deref().unwrap_or(&[]);
    let (dev, ino) = pending_dir.create_stage_file(&stage_name, stage_bytes)?;
    let (parent, lock_leaf) =
        git_meta.open_parent_dir(spec.lock_rel_path.as_slice(), spec.create_parents)?;
    let linked = pending_dir.link_to(&stage_name, parent.as_fd(), OsStr::from_bytes(&lock_leaf))?;
    if !linked {
        let existing = git_meta.stat_regular_dev_ino(spec.lock_rel_path.as_slice())?;
        if existing != Some((dev, ino)) {
            let _ = pending_dir.remove_if_exists(&stage_name);
            return Err(foreign_lock_error(spec.lock_rel_path.as_slice()));
        }
    }
    fsync(parent.as_fd()).map_err(std::io::Error::from)?;
    commit_locked_metadata(ctx.git_dir, spec)?;
    pending_dir.remove_if_exists(&stage_name)?;
    Ok(())
}

fn verify_all_at_before(ctx: &TxContext<'_>, actions: &[Action]) -> Result<()> {
    let worktree = CheckedRoot::open(ctx.workdir)?;
    let git_meta = CheckedRoot::open(ctx.git_dir)?;
    for action in actions {
        match action {
            Action::RehydrateObject { .. } => {}
            Action::WriteWorktreeEntry {
                path,
                expected_before,
                ..
            } => {
                if read_entry_state(&worktree, path.as_slice(), ctx.policy.max_file_bytes)?
                    != *expected_before
                {
                    return Err(divergence_error(path.as_slice()));
                }
            }
            Action::RemoveWorktreeEntry {
                path,
                expected_before,
            } => {
                if read_entry_state(&worktree, path.as_slice(), ctx.policy.max_file_bytes)?
                    != Some(*expected_before)
                {
                    return Err(divergence_error(path.as_slice()));
                }
            }
            Action::EnsureDir { path } => {
                if matches!(
                    worktree.entry_kind(path.as_slice())?,
                    Some(k) if k != PathEntryKind::Directory
                ) {
                    return Err(divergence_error(path.as_slice()));
                }
            }
            Action::RemoveEmptyDir { path } => {
                if worktree.entry_kind(path.as_slice())? != Some(PathEntryKind::Directory) {
                    return Err(divergence_error(path.as_slice()));
                }
            }
            _ => {
                let spec = metadata_spec(ctx.store, action)?.expect("metadata variant");
                if git_meta.read_file_raw(spec.rel_path.as_slice(), MAX_CAS_BYTES as u64)?
                    != spec.expected_before
                {
                    return Err(divergence_error(spec.rel_path.as_slice()));
                }
            }
        }
    }
    Ok(())
}

fn read_entry_state(root: &CheckedRoot, path: &[u8], limit: u64) -> Result<Option<EntryState>> {
    Ok(root.read_entry(path, limit)?.map(|entry| EntryState {
        kind: entry.kind,
        content: ContentId::for_bytes(&entry.bytes),
    }))
}

fn verify_lock_if_present(git_dir: &Path, owner: &LockOwner) -> Result<()> {
    let git_meta = CheckedRoot::open(git_dir)?;
    if let Some((dev, ino)) = git_meta.stat_regular_dev_ino(owner.lock_rel_path.as_slice())?
        && (dev, ino) != (owner.dev, owner.ino)
    {
        return Err(foreign_lock_error(owner.lock_rel_path.as_slice()));
    }
    Ok(())
}

fn release_owned_lock(ctx: &TxContext<'_>, owner: &LockOwner) -> Result<()> {
    let git_meta = CheckedRoot::open(ctx.git_dir)?;
    if let Some((dev, ino)) = git_meta.stat_regular_dev_ino(owner.lock_rel_path.as_slice())? {
        if (dev, ino) != (owner.dev, owner.ino) {
            return Err(foreign_lock_error(owner.lock_rel_path.as_slice()));
        }
        let (parent, lock_leaf) =
            git_meta.open_parent_dir(owner.lock_rel_path.as_slice(), false)?;
        match unlinkat(
            parent.as_fd(),
            OsStr::from_bytes(&lock_leaf),
            AtFlags::empty(),
        ) {
            Ok(()) | Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(std::io::Error::from(error).into()),
        }
        fsync(parent.as_fd()).map_err(std::io::Error::from)?;
    }
    ctx.store
        .pending_dir()?
        .remove_if_exists(stage_name_str(&owner.stage_name)?)?;
    Ok(())
}

fn stage_name_str(name: &BString) -> Result<&str> {
    std::str::from_utf8(name.as_slice())
        .map_err(|_| Error::CorruptRecovery("pending stage filename is not UTF-8".to_owned()))
}

fn cleanup_worktree_temps(ctx: &TxContext<'_>, actions: &[Action]) -> Result<()> {
    let root = CheckedRoot::open(ctx.workdir)?;
    for (idx, action) in actions.iter().enumerate() {
        if let Action::WriteWorktreeEntry { path, .. } = action {
            root.cleanup_temp(path.as_slice(), &worktree_temp_name(ctx.tx_id, idx))?;
        }
    }
    Ok(())
}

fn cleanup_pending_state(ctx: &TxContext<'_>, actions: &[Action]) -> Result<()> {
    cleanup_worktree_temps(ctx, actions)?;
    let pending_dir = ctx.store.pending_dir()?;
    pending_dir.remove_if_exists(ACTIVE_RECORD)?;
    cleanup_orphan_pending(ctx.store, &pending_dir)?;
    Ok(())
}

fn cleanup_orphan_pending(
    store: &RecoveryStore,
    pending_dir: &crate::recovery::disk::Directory,
) -> Result<()> {
    pending_dir.remove_if_exists(PLAN_RECORD)?;
    let pending_path = store.root().join("pending");
    for entry in fs::read_dir(&pending_path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("stage_") {
            pending_dir.remove_if_exists(&name)?;
        }
    }
    Ok(())
}

fn stage_file_name(tx_id: ContentId, idx: usize) -> String {
    let hex = tx_id.to_hex();
    format!("stage_{}_{idx}", &hex[..16])
}

fn worktree_temp_name(tx_id: ContentId, idx: usize) -> String {
    let hex = tx_id.to_hex();
    format!(".git2-tmp-{}-{idx}", &hex[..16])
}

fn foreign_lock_error(path: &[u8]) -> Error {
    Error::UnsupportedRepository(format!(
        "refusing unknown Git lock {:?}",
        BString::from(path)
    ))
}

fn divergence_error(path: &[u8]) -> Error {
    Error::UnsupportedRepository(format!(
        "repository state diverged at {:?}",
        BString::from(path)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_and_plan_records_round_trip_and_reject_corruption() {
        let actions = vec![
            Action::RehydrateObject {
                oid: GitOid::from_bytes([0x11; 20]),
                content: ContentId::for_bytes(b"obj"),
            },
            Action::WriteWorktreeEntry {
                path: BString::from(b"dir/file\xff".to_vec()),
                expected_before: None,
                target: EntryState {
                    kind: FileKind::Executable,
                    content: ContentId::for_bytes(b"exec"),
                },
            },
            Action::RemoveWorktreeEntry {
                path: BString::from(b"old".to_vec()),
                expected_before: EntryState {
                    kind: FileKind::Regular,
                    content: ContentId::for_bytes(b"old"),
                },
            },
            Action::EnsureDir {
                path: BString::from(b"dir".to_vec()),
            },
            Action::RemoveEmptyDir {
                path: BString::from(b"gone".to_vec()),
            },
            Action::WriteIndex {
                expected_before: Some(ContentId::for_bytes(b"idx1")),
                target: ContentId::for_bytes(b"idx2"),
            },
            Action::RemoveIndex {
                expected_before: ContentId::for_bytes(b"idx2"),
            },
            Action::WriteRef {
                name: BString::from(b"refs/heads/main".to_vec()),
                expected_before: None,
                target: BString::from(b"0000000000000000000000000000000000000000\n".to_vec()),
            },
            Action::RemoveRef {
                name: BString::from(b"refs/heads/tmp".to_vec()),
                expected_before: BString::from(
                    b"0000000000000000000000000000000000000000\n".to_vec(),
                ),
            },
            Action::WritePackedRefs {
                expected_before: None,
                target: Some(BString::from(b"# pack-refs\n".to_vec())),
            },
            Action::WriteHead {
                expected_before: BString::from(b"ref: refs/heads/main\n".to_vec()),
                target: BString::from(b"ref: refs/heads/other\n".to_vec()),
            },
        ];
        let encoded_plan = encode_plan(&actions).unwrap();
        assert_eq!(decode_plan(&encoded_plan).unwrap(), actions);
        for end in 0..encoded_plan.len() {
            assert!(decode_plan(&encoded_plan[..end]).is_err());
        }
        let mut trailing_plan = encoded_plan.clone();
        trailing_plan.push(0);
        assert!(decode_plan(&trailing_plan).is_err());

        let record = PendingRecord {
            tx_id: ContentId::for_bytes(b"tx"),
            parent: ContentId::for_bytes(b"parent"),
            before_view: ContentId::for_bytes(b"before"),
            target_view: ContentId::for_bytes(b"after"),
            plan_id: ContentId::for_bytes(&encoded_plan),
            timestamp_seconds: -42,
            command: BString::from("synthetic"),
            phase: PendingPhase::Applying,
            next_action: 5,
            lock_owner: Some(LockOwner {
                action_index: 5,
                dev: 7,
                ino: 99,
                lock_rel_path: BString::from("index.lock"),
                stage_name: BString::from("stage_deadbeef_5"),
            }),
        };
        let encoded_record = record.encode().unwrap();
        assert_eq!(PendingRecord::decode(&encoded_record).unwrap(), record);
        for end in 0..encoded_record.len() {
            assert!(PendingRecord::decode(&encoded_record[..end]).is_err());
        }
        let mut trailing_record = encoded_record;
        trailing_record.push(0);
        assert!(PendingRecord::decode(&trailing_record).is_err());
    }
}
