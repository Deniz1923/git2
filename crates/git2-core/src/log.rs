//! Bounded first-parent commit history walk (`git2 log`).

use std::collections::BTreeSet;

use crate::object::{Commit, GitOid, ObjectKind};
use crate::paths::CheckedRoot;
use crate::recovery::MAX_CAS_BYTES;
use crate::repository::LocalRepository;
use crate::reset::{resolve_ref_chain, trim_newline};
use crate::retention::MAX_OBJECT_BYTES;
use crate::store::ObjectStore;
use crate::{Error, Result};

/// Default maximum number of commits returned by `git2 log`.
pub const DEFAULT_LOG_COUNT: usize = 50;

/// Hard upper bound on a single `git2 log` walk.
pub const MAX_LOG_COUNT: usize = 10_000;

/// Options controlling bounded history traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogOptions {
    pub max_count: usize,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            max_count: DEFAULT_LOG_COUNT,
        }
    }
}

/// One commit yielded by [`walk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub oid: GitOid,
    pub commit: Commit,
}

impl LogEntry {
    /// Format this commit in Git-style log text without assuming UTF-8 in message or author bytes.
    #[must_use]
    pub fn format_text(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"commit ");
        out.extend_from_slice(self.oid.to_hex().as_bytes());
        out.extend_from_slice(b"\nAuthor: ");
        out.extend_from_slice(self.commit.author.name.as_ref());
        out.extend_from_slice(b" <");
        out.extend_from_slice(self.commit.author.email.as_ref());
        out.extend_from_slice(b">\nDate:   ");
        let date = format_git_date(
            self.commit.author.timestamp,
            self.commit.author.timezone_offset_minutes,
        );
        out.extend_from_slice(date.as_bytes());
        out.extend_from_slice(b"\n\n");

        let body: &[u8] = self.commit.message.as_ref();
        let trimmed = body.strip_suffix(b"\n").unwrap_or(body);
        for line in trimmed.split(|&byte| byte == b'\n') {
            if line.is_empty() {
                out.push(b'\n');
            } else {
                out.extend_from_slice(b"    ");
                out.extend_from_slice(line);
                out.push(b'\n');
            }
        }
        out.push(b'\n');
        out
    }
}

fn format_git_date(timestamp: i64, offset_minutes: i16) -> String {
    let time = gix::date::Time::new(timestamp, i32::from(offset_minutes) * 60);
    time.format(gix::date::time::format::DEFAULT)
        .unwrap_or_else(|_| {
            let sign = if offset_minutes < 0 { '-' } else { '+' };
            let abs = offset_minutes.unsigned_abs();
            format!("{timestamp} {sign}{:02}{:02}", abs / 60, abs % 60)
        })
}

/// Walk at most `options.max_count` commits along first-parent history starting at `HEAD`.
///
/// Missing or corrupt commits in the walked prefix return an error instead of truncating
/// history silently.
///
/// # Errors
///
/// Returns an error if `options.max_count` exceeds [`MAX_LOG_COUNT`], `HEAD` is missing or
/// unborn, a traversed commit object is missing or corrupt, or a parent cycle exists.
pub fn walk(repo: &LocalRepository, options: LogOptions) -> Result<Vec<LogEntry>> {
    if options.max_count > MAX_LOG_COUNT {
        return Err(Error::UnsupportedRepository(format!(
            "requested log count {} exceeds maximum {MAX_LOG_COUNT}",
            options.max_count
        )));
    }
    let meta = CheckedRoot::open(repo.git_dir())?;
    let mut current = resolve_head_commit(&meta)?;
    if options.max_count == 0 {
        return Ok(Vec::new());
    }

    let objects = ObjectStore::open(repo.git_dir())?;
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    while let Some(oid) = current {
        if entries.len() >= options.max_count {
            break;
        }
        if !seen.insert(oid) {
            return Err(Error::CorruptObject(format!(
                "cycle in commit parent graph at {oid}"
            )));
        }
        let raw = reader.read(oid)?;
        if raw.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "object {oid} is {}, expected commit",
                raw.kind
            )));
        }
        let commit = Commit::parse(&raw.data)?;
        current = commit.parents.first().copied();
        entries.push(LogEntry { oid, commit });
    }
    Ok(entries)
}

fn resolve_head_commit(meta: &CheckedRoot) -> Result<Option<GitOid>> {
    let raw_head = meta
        .read_file_raw(b"HEAD", MAX_CAS_BYTES as u64)?
        .ok_or_else(|| Error::UnsupportedRepository("Git HEAD is missing".to_owned()))?;
    let trimmed = trim_newline(&raw_head);
    if let Some(target_ref) = trimmed.strip_prefix(b"ref: ") {
        let (leaf_ref, oid) = resolve_ref_chain(meta, target_ref)?;
        let oid = oid.ok_or_else(|| {
            Error::UnsupportedRepository(format!(
                "current branch {} does not have any commits yet",
                String::from_utf8_lossy(leaf_ref.as_slice())
            ))
        })?;
        return Ok(Some(oid));
    }
    Ok(Some(GitOid::from_hex(trimmed)?))
}
