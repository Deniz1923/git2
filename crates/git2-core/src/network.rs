//! Network and remote operations (`clone`, `fetch`, `pull`, `push`) via strict `git` argv delegation.
//!
//! `git2` delegates transport, authentication, negotiation, and protocol handling to the
//! installed `git` executable (ADR D013) while owning validation, privacy invariants,
//! delegated pending transactions, `oplog` entries, and conflict-safe rollback.

use std::ffi::{OsStr, OsString};
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use bstr::BString;

use crate::recovery::{ContentId, RecoveryStore};
use crate::repository::{AdoptOptions, Adoption, LocalRepository};
use crate::transaction::{self, DelegatedExit, Failpoint};
use crate::{Error, Result};

const MAX_COMMAND_BYTES: usize = 4096;

/// Output captured from a delegated `git` invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitCommandOutput {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

/// Options for [`clone_repo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOptions {
    pub url: String,
    pub directory: Option<PathBuf>,
    pub branch: Option<String>,
    pub origin: Option<String>,
    pub adopt: bool,
    pub max_file_bytes: u64,
}

impl Default for CloneOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            directory: None,
            branch: None,
            origin: None,
            adopt: true,
            max_file_bytes: AdoptOptions::default().max_file_bytes,
        }
    }
}

/// Outcome returned by [`clone_repo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneOutcome {
    pub workdir: PathBuf,
    pub adoption: Option<Adoption>,
}

/// Options for [`fetch`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchOptions {
    pub remote: Option<String>,
    pub refspecs: Vec<String>,
    pub all: bool,
    pub prune: bool,
    pub tags: bool,
}

/// Outcome returned by [`fetch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchOutcome {
    pub operation_id: Option<ContentId>,
    pub before_view: ContentId,
    pub after_view: ContentId,
}

/// Merge strategy for [`pull`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PullMode {
    /// Only allow fast-forward updates (`git pull --ff-only`, default).
    #[default]
    FfOnly,
    /// Allow non-fast-forward merges inside a conflict-recoverable transaction.
    Merge,
    /// Rebase local commits onto upstream inside a conflict-recoverable transaction.
    Rebase,
}

/// Options for [`pull`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullOptions {
    pub mode: PullMode,
    pub prune: bool,
    pub remote: Option<String>,
    pub refspecs: Vec<String>,
}

/// Outcome returned by [`pull`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullOutcome {
    pub operation_id: Option<ContentId>,
    pub before_view: ContentId,
    pub after_view: ContentId,
}

/// Options for [`push`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PushOptions {
    pub remote: Option<String>,
    pub refspecs: Vec<String>,
    pub set_upstream: bool,
    pub tags: bool,
    pub delete: bool,
    pub force_with_lease: bool,
    pub force: bool,
    pub dry_run: bool,
}

/// Outcome returned by [`push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOutcome {
    pub operation_id: Option<ContentId>,
    pub before_view: Option<ContentId>,
    pub after_view: Option<ContentId>,
}

/// Clone `options.url` via installed `git` and automatically adopt it unless `--no-adopt` is set.
///
/// # Errors
///
/// Returns an error if arguments or URLs fail validation, `git` is missing or exits non-zero,
/// or post-clone adoption fails (leaving the valid Git clone intact on disk).
pub fn clone_repo(cwd: &Path, options: &CloneOptions) -> Result<CloneOutcome> {
    reject_leading_dash("url", OsStr::new(&options.url))?;
    reject_ext_protocol(&options.url)?;
    if let Some(branch) = &options.branch {
        reject_leading_dash("branch", OsStr::new(branch))?;
    }
    if let Some(origin) = &options.origin {
        validate_remote_name(origin)?;
    }
    if options.adopt {
        let _ = AdoptOptions {
            max_file_bytes: options.max_file_bytes,
        }
        .validate()?;
    }

    let dest = resolve_clone_destination(cwd, &options.url, options.directory.as_deref())?;
    if let Ok(meta) = std::fs::symlink_metadata(&dest) {
        let non_empty_dir =
            meta.is_dir() && std::fs::read_dir(&dest)?.next().transpose()?.is_some();
        if !meta.is_dir() || non_empty_dir {
            return Err(Error::UnsupportedRepository(format!(
                "destination path {} already exists and is not an empty directory",
                dest.display()
            )));
        }
    }

    let mut args = vec![OsString::from("clone")];
    if let Some(branch) = &options.branch {
        args.push(OsString::from("--branch"));
        args.push(OsString::from(branch));
    }
    if let Some(origin) = &options.origin {
        args.push(OsString::from("--origin"));
        args.push(OsString::from(origin));
    }
    args.push(OsString::from("--"));
    args.push(OsString::from(&options.url));
    args.push(dest.as_os_str().to_os_string());

    run_git_streaming(Some(cwd), &args, "git clone")?;

    let adoption = if options.adopt {
        let repo = LocalRepository::discover(&dest).map_err(|error| {
            Error::UnsupportedRepository(format!(
                "cloned repository into {}, but automatic git2 adoption failed: {error}",
                dest.display()
            ))
        })?;
        let adopted = repo
            .adopt(AdoptOptions {
                max_file_bytes: options.max_file_bytes,
            })
            .map_err(|error| {
                Error::UnsupportedRepository(format!(
                    "cloned repository into {}, but automatic git2 adoption failed: {error}",
                    dest.display()
                ))
            })?;
        Some(adopted)
    } else {
        None
    };

    Ok(CloneOutcome {
        workdir: dest,
        adoption,
    })
}

/// Fetch objects and refs from a remote into an adopted repository.
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, receives unsafe
/// remote/refspec arguments, or `git fetch` fails.
pub fn fetch(repo: &LocalRepository, options: &FetchOptions) -> Result<FetchOutcome> {
    fetch_with_hook(repo, options, |_| Ok(()))
}

#[doc(hidden)]
pub fn fetch_with_hook(
    repo: &LocalRepository,
    options: &FetchOptions,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<FetchOutcome> {
    if options.all && (options.remote.is_some() || !options.refspecs.is_empty()) {
        return Err(Error::UnsupportedRepository(
            "cannot combine `--all` with an explicit remote or refspecs".to_owned(),
        ));
    }
    validate_remote_and_refspecs(options.remote.as_deref(), &options.refspecs, false)?;

    let mut args = vec![OsString::from("fetch")];
    if options.all {
        args.push(OsString::from("--all"));
    }
    if options.prune {
        args.push(OsString::from("--prune"));
    }
    if options.tags {
        args.push(OsString::from("--tags"));
    }
    push_remote_and_refspecs(&mut args, options.remote.as_deref(), &options.refspecs);

    let command = format_delegated_command(
        "fetch",
        &[
            options.all.then_some("--all"),
            options.prune.then_some("--prune"),
            options.tags.then_some("--tags"),
        ],
        options.remote.as_deref(),
        &options.refspecs,
    );

    let workdir = repo.workdir().to_path_buf();
    let outcome = transaction::run_delegated_with_hook(repo, command, false, hook, move || {
        let output = run_git_command(Some(&workdir), &args, false)?;
        if output.status.success() {
            Ok(DelegatedExit::Success)
        } else {
            Ok(DelegatedExit::Failed(format_git_failure(
                "git fetch",
                output.status,
                &output.stderr,
            )))
        }
    })?;

    Ok(FetchOutcome {
        operation_id: outcome.operation_id,
        before_view: outcome.before_view,
        after_view: outcome.after_view,
    })
}

/// Fetch from a remote and update the current branch (`--ff-only` by default).
///
/// # Errors
///
/// Returns an error if the repository is not adopted, has a pending transaction, receives unsafe
/// remote/refspec arguments, or `git pull` fails or leaves unresolved conflicts.
pub fn pull(repo: &LocalRepository, options: &PullOptions) -> Result<PullOutcome> {
    pull_with_hook(repo, options, |_| Ok(()))
}

#[doc(hidden)]
pub fn pull_with_hook(
    repo: &LocalRepository,
    options: &PullOptions,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<PullOutcome> {
    validate_remote_and_refspecs(options.remote.as_deref(), &options.refspecs, false)?;

    let mut args = vec![OsString::from("pull")];
    let mode_flag = match options.mode {
        PullMode::FfOnly => {
            args.push(OsString::from("--ff-only"));
            "--ff-only"
        }
        PullMode::Merge => {
            args.push(OsString::from("--no-rebase"));
            args.push(OsString::from("--no-edit"));
            "--no-rebase"
        }
        PullMode::Rebase => {
            args.push(OsString::from("--rebase"));
            "--rebase"
        }
    };
    if options.prune {
        args.push(OsString::from("--prune"));
    }
    push_remote_and_refspecs(&mut args, options.remote.as_deref(), &options.refspecs);

    let allow_conflict_rollback = !matches!(options.mode, PullMode::FfOnly);
    let command = format_delegated_command(
        "pull",
        &[Some(mode_flag), options.prune.then_some("--prune")],
        options.remote.as_deref(),
        &options.refspecs,
    );

    let workdir = repo.workdir().to_path_buf();
    let outcome = transaction::run_delegated_with_hook(
        repo,
        command,
        allow_conflict_rollback,
        hook,
        move || {
            let output = run_git_command(Some(&workdir), &args, false)?;
            if output.status.success() {
                Ok(DelegatedExit::Success)
            } else {
                Ok(DelegatedExit::Failed(format_git_failure(
                    "git pull",
                    output.status,
                    &output.stderr,
                )))
            }
        },
    )?;

    Ok(PullOutcome {
        operation_id: outcome.operation_id,
        before_view: outcome.before_view,
        after_view: outcome.after_view,
    })
}

/// Push local refs to a remote via installed `git`.
///
/// Remote pushes cannot be retracted by local `git2 undo`; on adopted repositories any updated
/// local tracking refs are recorded as non-undoable `git2:push` entries in `oplog`.
///
/// # Errors
///
/// Returns an error if a pending transaction exists, arguments or refspecs are invalid (including
/// any attempt to reference `refs/git2/`), or `git push` fails.
pub fn push(repo: &LocalRepository, options: &PushOptions) -> Result<PushOutcome> {
    push_with_hook(repo, options, |_| Ok(()))
}

#[doc(hidden)]
pub fn push_with_hook(
    repo: &LocalRepository,
    options: &PushOptions,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<PushOutcome> {
    validate_remote_and_refspecs(options.remote.as_deref(), &options.refspecs, true)?;

    let mut args = vec![OsString::from("push")];
    if options.set_upstream {
        args.push(OsString::from("--set-upstream"));
    }
    if options.tags {
        args.push(OsString::from("--tags"));
    }
    if options.delete {
        args.push(OsString::from("--delete"));
    }
    if options.force_with_lease {
        args.push(OsString::from("--force-with-lease"));
    }
    if options.force {
        args.push(OsString::from("--force"));
    }
    if options.dry_run {
        args.push(OsString::from("--dry-run"));
    }
    push_remote_and_refspecs(&mut args, options.remote.as_deref(), &options.refspecs);

    if !RecoveryStore::exists(repo.git_dir())? {
        run_git_streaming(Some(repo.workdir()), &args, "git push")?;
        return Ok(PushOutcome {
            operation_id: None,
            before_view: None,
            after_view: None,
        });
    }

    if options.dry_run {
        let store = RecoveryStore::open(repo.git_dir())?;
        if store.has_pending()? {
            return Err(Error::UnsupportedRepository(
                "a pending transaction exists; reconcile or inspect it with `recover` first"
                    .to_owned(),
            ));
        }
        run_git_streaming(Some(repo.workdir()), &args, "git push")?;
        return Ok(PushOutcome {
            operation_id: None,
            before_view: None,
            after_view: None,
        });
    }

    let command = format_delegated_command(
        "push",
        &[
            options.set_upstream.then_some("--set-upstream"),
            options.tags.then_some("--tags"),
            options.delete.then_some("--delete"),
            options.force_with_lease.then_some("--force-with-lease"),
            options.force.then_some("--force"),
        ],
        options.remote.as_deref(),
        &options.refspecs,
    );

    let workdir = repo.workdir().to_path_buf();
    let outcome = transaction::run_delegated_with_hook(repo, command, false, hook, move || {
        let output = run_git_command(Some(&workdir), &args, false)?;
        if output.status.success() {
            Ok(DelegatedExit::Success)
        } else {
            Ok(DelegatedExit::Failed(format_git_failure(
                "git push",
                output.status,
                &output.stderr,
            )))
        }
    })?;

    Ok(PushOutcome {
        operation_id: outcome.operation_id,
        before_view: Some(outcome.before_view),
        after_view: Some(outcome.after_view),
    })
}

pub(crate) fn run_git_command(
    cwd: Option<&Path>,
    args: &[OsString],
    capture_stdout: bool,
) -> Result<GitCommandOutput> {
    let mut cmd = Command::new("git");
    cmd.arg("-c").arg("protocol.ext.allow=never").args(args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if capture_stdout {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = cmd.output().map_err(map_git_spawn_error)?;
        Ok(GitCommandOutput {
            status: output.status,
            stdout: output.stdout,
            stderr: output.stderr,
        })
    } else {
        cmd.stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = cmd.status().map_err(map_git_spawn_error)?;
        Ok(GitCommandOutput {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }
}

pub(crate) fn run_git_streaming(cwd: Option<&Path>, args: &[OsString], label: &str) -> Result<()> {
    let output = run_git_command(cwd, args, false)?;
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::UnsupportedRepository(format_git_failure(
            label,
            output.status,
            &output.stderr,
        )))
    }
}

pub(crate) fn format_git_failure(label: &str, status: ExitStatus, stderr: &[u8]) -> String {
    let detail = String::from_utf8_lossy(stderr).trim().to_owned();
    if detail.is_empty() {
        match status.code() {
            Some(code) => format!("{label} exited with status {code}"),
            None => format!("{label} terminated by signal"),
        }
    } else {
        format!("{label} failed: {detail}")
    }
}

fn map_git_spawn_error(error: std::io::Error) -> Error {
    if error.kind() == ErrorKind::NotFound {
        Error::UnsupportedRepository(
            "git executable not found on PATH; network and remote commands require installed Git"
                .to_owned(),
        )
    } else {
        Error::Io(error)
    }
}

pub(crate) fn reject_leading_dash(label: &str, value: &OsStr) -> Result<()> {
    let bytes = value.as_bytes();
    if bytes.is_empty() {
        return Err(Error::UnsupportedRepository(format!(
            "{label} cannot be empty"
        )));
    }
    if bytes.contains(&0) {
        return Err(Error::UnsupportedRepository(format!(
            "{label} cannot contain NUL bytes"
        )));
    }
    if bytes.starts_with(b"-") {
        return Err(Error::UnsupportedRepository(format!(
            "{label} {:?} must not start with `-`",
            value.to_string_lossy()
        )));
    }
    Ok(())
}

pub(crate) fn reject_ext_protocol(url: &str) -> Result<()> {
    if url.len() >= 5 && url[..5].eq_ignore_ascii_case("ext::") {
        return Err(Error::UnsupportedRepository(
            "`ext::` remote helpers are forbidden".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_remote_name(name: &str) -> Result<()> {
    reject_leading_dash("remote name", OsStr::new(name))?;
    let probe = format!("refs/remotes/{name}/HEAD");
    gix::validate::reference::name(probe.as_bytes().into())
        .map_err(|_| Error::UnsupportedRepository(format!("invalid remote name {name:?}")))?;
    Ok(())
}

pub(crate) fn reject_private_refspecs(refspecs: &[String]) -> Result<()> {
    for spec in refspecs {
        reject_leading_dash("refspec", OsStr::new(spec))?;
        if spec.contains("refs/git2") || spec.contains(".git/git2") {
            return Err(Error::UnsupportedRepository(format!(
                "refspec {spec:?} cannot reference private `refs/git2/` or `.git/git2` state"
            )));
        }
    }
    Ok(())
}

/// Redact embedded credentials (`scheme://user:pass@host`) before persisting in `oplog`.
#[must_use]
pub fn sanitize_url_for_oplog(input: &str) -> String {
    let Some(scheme_end) = input.find("://") else {
        return input.to_owned();
    };
    let auth_start = scheme_end + 3;
    let rest = &input[auth_start..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let Some(at_rel) = authority.rfind('@') else {
        return input.to_owned();
    };
    let mut out = String::with_capacity(input.len());
    out.push_str(&input[..auth_start]);
    out.push_str("***@");
    out.push_str(&rest[at_rel + 1..]);
    out
}

pub(crate) fn format_delegated_command(
    subcommand: &str,
    flags: &[Option<&str>],
    remote: Option<&str>,
    args: &[String],
) -> BString {
    let mut out = String::from(subcommand);
    for flag in flags.iter().flatten() {
        out.push(' ');
        out.push_str(flag);
    }
    if let Some(remote) = remote {
        append_bounded_token(&mut out, &sanitize_url_for_oplog(remote));
    }
    for arg in args {
        append_bounded_token(&mut out, &sanitize_url_for_oplog(arg));
    }
    BString::from(out)
}

fn append_bounded_token(out: &mut String, token: &str) {
    if out.len() + 1 + token.len() > MAX_COMMAND_BYTES {
        if !out.ends_with(" ...") {
            out.push_str(" ...");
        }
        return;
    }
    out.push(' ');
    out.push_str(token);
}

fn validate_remote_and_refspecs(
    remote: Option<&str>,
    refspecs: &[String],
    _allow_url_remote: bool,
) -> Result<()> {
    if remote.is_none() && !refspecs.is_empty() {
        return Err(Error::UnsupportedRepository(
            "explicit refspecs require a remote name or URL".to_owned(),
        ));
    }
    if let Some(remote) = remote {
        reject_leading_dash("remote", OsStr::new(remote))?;
        reject_ext_protocol(remote)?;
    }
    reject_private_refspecs(refspecs)?;
    Ok(())
}

fn push_remote_and_refspecs(args: &mut Vec<OsString>, remote: Option<&str>, refspecs: &[String]) {
    if let Some(remote) = remote {
        args.push(OsString::from("--"));
        args.push(OsString::from(remote));
        for spec in refspecs {
            args.push(OsString::from(spec));
        }
    }
}

fn resolve_clone_destination(cwd: &Path, url: &str, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(if dir.is_absolute() {
            dir.to_path_buf()
        } else {
            cwd.join(dir)
        });
    }
    let trimmed = url.trim_end_matches('/');
    let trimmed = trimmed
        .strip_suffix("/.git")
        .or_else(|| trimmed.strip_suffix(".git"))
        .unwrap_or(trimmed);
    let leaf = trimmed
        .rsplit(['/', ':'])
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| {
            Error::UnsupportedRepository(format!(
                "could not derive clone directory name from URL {url:?}; pass an explicit directory"
            ))
        })?;
    Ok(cwd.join(leaf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_url_strips_embedded_credentials_only() {
        assert_eq!(
            sanitize_url_for_oplog("https://alice:s3cr3t@example.invalid/org/repo.git"),
            "https://***@example.invalid/org/repo.git"
        );
        assert_eq!(
            sanitize_url_for_oplog("ssh://git@example.invalid/repo.git"),
            "ssh://***@example.invalid/repo.git"
        );
        assert_eq!(
            sanitize_url_for_oplog("https://example.invalid/org/repo@branch.git"),
            "https://example.invalid/org/repo@branch.git"
        );
        assert_eq!(sanitize_url_for_oplog("origin"), "origin");
    }

    #[test]
    fn guards_reject_ext_urls_leading_dash_and_private_refs() {
        assert!(reject_ext_protocol("ext::sh -c whoami").is_err());
        assert!(reject_ext_protocol("EXT::foo").is_err());
        assert!(reject_leading_dash("url", OsStr::new("--upload-pack=evil")).is_err());
        assert!(validate_remote_name("-bad").is_err());
        assert!(reject_private_refspecs(&["HEAD:refs/git2/leak".to_owned()]).is_err());
        assert!(reject_private_refspecs(&[":refs/heads/tmp".to_owned()]).is_ok());
    }
}
