//! Git-style comparisons between HEAD, the index, and the working tree.

use std::collections::BTreeMap;

use bstr::BString;

use crate::repository::LocalRepository;
use crate::{Error, Result};

/// One side of a two-column Git status entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    Added,
    Modified,
    Deleted,
    TypeChanged,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
}

impl StatusCode {
    /// Return the porcelain-v1 byte for this status.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        match self {
            Self::Added => b'A',
            Self::Modified => b'M',
            Self::Deleted => b'D',
            Self::TypeChanged => b'T',
            Self::Renamed => b'R',
            Self::Copied => b'C',
            Self::Unmerged => b'U',
            Self::Untracked => b'?',
        }
    }
}

/// Status for one byte-safe repository-relative path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub path: BString,
    pub staged: Option<StatusCode>,
    pub worktree: Option<StatusCode>,
}

/// A deterministic, path-sorted repository status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryStatus {
    pub entries: Vec<StatusEntry>,
}

impl RepositoryStatus {
    /// Return true when HEAD, the index, and the eligible working tree agree.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.entries.is_empty()
    }

    /// Encode Git-compatible two-column porcelain for simple non-rename entries.
    ///
    /// Paths are emitted as raw bytes. With `nul_terminated`, records end in NUL and are
    /// unambiguous for every Unix path; otherwise they end in a newline for readability.
    #[must_use]
    pub fn porcelain_v1(&self, nul_terminated: bool) -> Vec<u8> {
        let mut output = Vec::new();
        let terminator = if nul_terminated { 0 } else { b'\n' };
        for entry in &self.entries {
            if entry.staged.is_none() && entry.worktree == Some(StatusCode::Untracked) {
                output.extend_from_slice(b"??");
            } else {
                output.push(entry.staged.map_or(b' ', StatusCode::as_byte));
                output.push(entry.worktree.map_or(b' ', StatusCode::as_byte));
            }
            output.push(b' ');
            output.extend_from_slice(entry.path.as_ref());
            output.push(terminator);
        }
        output
    }
}

impl LocalRepository {
    /// Compare HEAD, the index, and the working tree.
    ///
    /// # Errors
    ///
    /// Returns an error if Git metadata, objects, ignores, or working-copy paths cannot be read.
    pub fn status(&self) -> Result<RepositoryStatus> {
        let statuses = self
            .inner
            .status(gix::progress::Discard)
            .map_err(|error| Error::Gix(error.to_string()))?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .index_worktree_submodules(None)
            .into_iter(Vec::<BString>::new())
            .map_err(|error| Error::Gix(error.to_string()))?;
        let mut entries = BTreeMap::<Vec<u8>, StatusEntry>::new();
        for status in statuses {
            let status = status.map_err(|error| Error::Gix(error.to_string()))?;
            match status {
                gix::status::Item::IndexWorktree(change) => {
                    let Some(summary) = change.summary() else {
                        continue;
                    };
                    let code = match summary {
                        gix::status::index_worktree::iter::Summary::Removed => StatusCode::Deleted,
                        gix::status::index_worktree::iter::Summary::Added => StatusCode::Untracked,
                        gix::status::index_worktree::iter::Summary::IntentToAdd => {
                            StatusCode::Added
                        }
                        gix::status::index_worktree::iter::Summary::Modified => {
                            StatusCode::Modified
                        }
                        gix::status::index_worktree::iter::Summary::TypeChange => {
                            StatusCode::TypeChanged
                        }
                        gix::status::index_worktree::iter::Summary::Renamed => StatusCode::Renamed,
                        gix::status::index_worktree::iter::Summary::Copied => StatusCode::Copied,
                        gix::status::index_worktree::iter::Summary::Conflict => {
                            StatusCode::Unmerged
                        }
                    };
                    let path = change.rela_path().to_vec();
                    entry_for(&mut entries, path).worktree = Some(code);
                }
                gix::status::Item::TreeIndex(change) => {
                    let code = match &change {
                        gix::diff::index::Change::Addition { .. } => StatusCode::Added,
                        gix::diff::index::Change::Deletion { .. } => StatusCode::Deleted,
                        gix::diff::index::Change::Modification {
                            previous_entry_mode,
                            entry_mode,
                            ..
                        } => {
                            if (*previous_entry_mode == gix::index::entry::Mode::SYMLINK)
                                == (*entry_mode == gix::index::entry::Mode::SYMLINK)
                            {
                                StatusCode::Modified
                            } else {
                                StatusCode::TypeChanged
                            }
                        }
                        gix::diff::index::Change::Rewrite { copy, .. } => {
                            if *copy {
                                StatusCode::Copied
                            } else {
                                StatusCode::Renamed
                            }
                        }
                    };
                    let path = change.location().to_vec();
                    entry_for(&mut entries, path).staged = Some(code);
                }
            }
        }
        let mut entries: Vec<StatusEntry> = entries.into_values().collect();
        entries.sort_by(|left, right| {
            let left_untracked =
                left.staged.is_none() && left.worktree == Some(StatusCode::Untracked);
            let right_untracked =
                right.staged.is_none() && right.worktree == Some(StatusCode::Untracked);
            left_untracked
                .cmp(&right_untracked)
                .then_with(|| left.path.cmp(&right.path))
        });
        Ok(RepositoryStatus { entries })
    }
}

fn entry_for(entries: &mut BTreeMap<Vec<u8>, StatusEntry>, path: Vec<u8>) -> &mut StatusEntry {
    entries.entry(path.clone()).or_insert_with(|| StatusEntry {
        path: BString::from(path),
        staged: None,
        worktree: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_is_byte_safe_and_handles_untracked() {
        let status = RepositoryStatus {
            entries: vec![
                StatusEntry {
                    path: BString::from(vec![b'a', 0xff]),
                    staged: Some(StatusCode::Modified),
                    worktree: Some(StatusCode::Deleted),
                },
                StatusEntry {
                    path: BString::from("new"),
                    staged: None,
                    worktree: Some(StatusCode::Untracked),
                },
            ],
        };
        assert_eq!(status.porcelain_v1(true), b"MD a\xff\0?? new\0");
    }
}
