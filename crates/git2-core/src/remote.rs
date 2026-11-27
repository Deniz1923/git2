//! Remote management commands (`remote`, `add`, `remove`, `rename`, `get-url`, `set-url`).

use std::ffi::{OsStr, OsString};

use bstr::BString;

use crate::network::{
    format_delegated_command, format_git_failure, reject_ext_protocol, reject_leading_dash,
    run_git_command, validate_remote_name,
};
use crate::recovery::RecoveryStore;
use crate::repository::LocalRepository;
use crate::transaction::{self, DelegatedExit, Failpoint};
use crate::{Error, Result};

/// Supported `git2 remote` subcommands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteAction {
    List {
        verbose: bool,
    },
    Add {
        name: String,
        url: String,
    },
    Remove {
        name: String,
    },
    Rename {
        old: String,
        new: String,
    },
    GetUrl {
        name: String,
        push: bool,
        all: bool,
    },
    SetUrl {
        name: String,
        new_url: String,
        old_url: Option<String>,
        push: bool,
    },
}

/// Output produced by read-only remote inspection commands (`List` and `GetUrl`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteOutput {
    pub stdout: Vec<u8>,
}

/// Run a `git2 remote` command against `repo`.
///
/// For `remove` and `rename` on an adopted repository, this runs inside a delegated transaction so
/// removed or renamed `refs/remotes/<name>/*` tracking refs are captured in `oplog`. `.git/config`
/// remains outside v0.1 recovery views.
///
/// # Errors
///
/// Returns an error if a pending transaction exists, names or URLs fail validation, or `git remote`
/// exits non-zero.
pub fn run(repo: &LocalRepository, action: &RemoteAction) -> Result<RemoteOutput> {
    run_with_hook(repo, action, |_| Ok(()))
}

#[doc(hidden)]
pub fn run_with_hook(
    repo: &LocalRepository,
    action: &RemoteAction,
    hook: impl FnMut(Failpoint) -> Result<()>,
) -> Result<RemoteOutput> {
    validate_remote_action(action)?;
    if RecoveryStore::exists(repo.git_dir())? {
        let store = RecoveryStore::open(repo.git_dir())?;
        if store.has_pending()? {
            return Err(Error::UnsupportedRepository(
                "a pending transaction exists; reconcile or inspect it with `recover` first"
                    .to_owned(),
            ));
        }
    }

    let args = build_remote_args(action);
    match action {
        RemoteAction::Remove { .. } | RemoteAction::Rename { .. }
            if RecoveryStore::exists(repo.git_dir())? =>
        {
            let command = format_remote_command(action);
            let workdir = repo.workdir().to_path_buf();
            transaction::run_delegated_with_hook(repo, command, false, hook, move || {
                let output = run_git_command(Some(&workdir), &args, false)?;
                if output.status.success() {
                    Ok(DelegatedExit::Success)
                } else {
                    Ok(DelegatedExit::Failed(format_git_failure(
                        "git remote",
                        output.status,
                        &output.stderr,
                    )))
                }
            })?;
            Ok(RemoteOutput::default())
        }
        RemoteAction::List { .. } | RemoteAction::GetUrl { .. } => {
            let output = run_git_command(Some(repo.workdir()), &args, true)?;
            if output.status.success() {
                Ok(RemoteOutput {
                    stdout: output.stdout,
                })
            } else {
                Err(Error::UnsupportedRepository(format_git_failure(
                    "git remote",
                    output.status,
                    &output.stderr,
                )))
            }
        }
        _ => {
            let output = run_git_command(Some(repo.workdir()), &args, false)?;
            if output.status.success() {
                Ok(RemoteOutput::default())
            } else {
                Err(Error::UnsupportedRepository(format_git_failure(
                    "git remote",
                    output.status,
                    &output.stderr,
                )))
            }
        }
    }
}

fn validate_remote_action(action: &RemoteAction) -> Result<()> {
    match action {
        RemoteAction::List { .. } => Ok(()),
        RemoteAction::Add { name, url } => {
            validate_remote_name(name)?;
            reject_leading_dash("url", OsStr::new(url))?;
            reject_ext_protocol(url)
        }
        RemoteAction::Remove { name } | RemoteAction::GetUrl { name, .. } => {
            validate_remote_name(name)
        }
        RemoteAction::Rename { old, new } => {
            validate_remote_name(old)?;
            validate_remote_name(new)
        }
        RemoteAction::SetUrl {
            name,
            new_url,
            old_url,
            ..
        } => {
            validate_remote_name(name)?;
            reject_leading_dash("url", OsStr::new(new_url))?;
            reject_ext_protocol(new_url)?;
            if let Some(old) = old_url {
                reject_leading_dash("oldurl", OsStr::new(old))?;
            }
            Ok(())
        }
    }
}

fn build_remote_args(action: &RemoteAction) -> Vec<OsString> {
    let mut args = vec![OsString::from("remote")];
    match action {
        RemoteAction::List { verbose } => {
            if *verbose {
                args.push(OsString::from("-v"));
            }
        }
        RemoteAction::Add { name, url } => {
            args.push(OsString::from("add"));
            args.push(OsString::from("--"));
            args.push(OsString::from(name));
            args.push(OsString::from(url));
        }
        RemoteAction::Remove { name } => {
            args.push(OsString::from("remove"));
            args.push(OsString::from("--"));
            args.push(OsString::from(name));
        }
        RemoteAction::Rename { old, new } => {
            args.push(OsString::from("rename"));
            args.push(OsString::from("--"));
            args.push(OsString::from(old));
            args.push(OsString::from(new));
        }
        RemoteAction::GetUrl { name, push, all } => {
            args.push(OsString::from("get-url"));
            if *push {
                args.push(OsString::from("--push"));
            }
            if *all {
                args.push(OsString::from("--all"));
            }
            args.push(OsString::from("--"));
            args.push(OsString::from(name));
        }
        RemoteAction::SetUrl {
            name,
            new_url,
            old_url,
            push,
        } => {
            args.push(OsString::from("set-url"));
            if *push {
                args.push(OsString::from("--push"));
            }
            args.push(OsString::from("--"));
            args.push(OsString::from(name));
            args.push(OsString::from(new_url));
            if let Some(old) = old_url {
                args.push(OsString::from(old));
            }
        }
    }
    args
}

fn format_remote_command(action: &RemoteAction) -> BString {
    match action {
        RemoteAction::Remove { name } => {
            format_delegated_command("remote", &[Some("remove")], Some(name), &[])
        }
        RemoteAction::Rename { old, new } => format_delegated_command(
            "remote",
            &[Some("rename")],
            Some(old),
            std::slice::from_ref(new),
        ),
        _ => BString::from("remote"),
    }
}
