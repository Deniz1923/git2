//! Git-compatible local repository primitives and experimental recovery capture machinery.
//!
//! The crate is private to the git2 workspace. Its interfaces can evolve before 1.0, but its
//! on-disk Git writes must always remain compatible with ordinary Git. Recovery views can be
//! captured, inspected, and restored, with native `reset` and `undo` building on the same
//! verified journal before `add` and `commit`.

pub mod object;
#[cfg(unix)]
pub mod paths;
#[cfg(unix)]
pub mod recovery;
#[cfg(unix)]
pub mod repository;
#[cfg(unix)]
pub mod reset;
#[cfg(unix)]
pub mod restore;
#[cfg(unix)]
pub mod retention;
#[cfg(unix)]
pub mod stage;
#[cfg(unix)]
pub mod status;
pub mod store;
#[cfg(unix)]
pub mod transaction;
#[cfg(unix)]
pub mod undo;

/// Errors returned by the local repository core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid SHA-1 object id: {0}")]
    InvalidOid(String),

    #[error("corrupt Git object: {0}")]
    CorruptObject(String),

    #[error("Git object {0} was not found")]
    ObjectNotFound(object::GitOid),

    #[error("unsupported repository: {0}")]
    UnsupportedRepository(String),

    #[error("corrupt git2 recovery data: {0}")]
    CorruptRecovery(String),

    #[error("gix error: {0}")]
    Gix(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
