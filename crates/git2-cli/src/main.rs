//! Command-line entry point for the experimental git2 client.

use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
#[cfg(unix)]
use git2_core::commit::{self, CommitOutcome};
#[cfg(unix)]
use git2_core::log::{self, DEFAULT_LOG_COUNT, LogOptions};
#[cfg(unix)]
use git2_core::network::{
    self, CloneOptions, FetchOptions, FetchOutcome, PullMode, PullOptions, PullOutcome,
    PushOptions, PushOutcome,
};
use git2_core::object::{GitOid, ObjectKind, hash_object};
#[cfg(unix)]
use git2_core::recovery::{ContentId, OperationKind, RecoveryStore};
#[cfg(unix)]
use git2_core::remote::{self, RemoteAction, RemoteOutput};
#[cfg(unix)]
use git2_core::repository::{AdoptOptions, LocalRepository};
#[cfg(unix)]
use git2_core::reset::{self, ResetMode, ResetOutcome};
#[cfg(unix)]
use git2_core::restore;
#[cfg(unix)]
use git2_core::stage::{self, AddOutcome};
use git2_core::store::ObjectStore;
#[cfg(unix)]
use git2_core::transaction::{
    self, Change, PendingPhase, RecoveryOutcome, RecoveryRepository, format_change_line,
};
#[cfg(unix)]
use git2_core::undo::{self, UndoOutcome};
#[cfg(all(unix, debug_assertions))]
use git2_core::{Error, transaction::Failpoint};

#[cfg(unix)]
const WARN_STORE_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// A learning-first Git-compatible client with local recovery.
#[derive(Parser)]
#[command(name = "git2", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Capture a recovery baseline for an existing supported Git repository.
    #[cfg(unix)]
    Adopt {
        /// Refuse eligible files larger than this many MiB.
        #[arg(long, default_value_t = 100)]
        max_file_mib: u64,
    },

    /// Save an explicit checkpoint of an adopted repository.
    #[cfg(unix)]
    Checkpoint,

    /// Stage literal file or directory paths (including deletions) or `--all` into the index.
    #[cfg(unix)]
    Add {
        /// Stage all non-ignored and tracked changes, including deletions, across the repository.
        #[arg(short = 'A', long, conflicts_with = "paths")]
        all: bool,

        /// Literal files or directories relative to the current directory (use `--` for leading `-`).
        #[arg(value_name = "PATH")]
        paths: Vec<OsString>,
    },

    /// Record staged changes as a new commit on HEAD or the current branch.
    #[cfg(unix)]
    Commit {
        /// Commit message describing the staged snapshot.
        #[arg(short = 'm', long = "message", value_name = "MESSAGE")]
        message: String,
    },

    /// Show bounded first-parent commit history reachable from HEAD.
    #[cfg(unix)]
    Log {
        /// Limit the number of commits shown (default 50, max 10000).
        #[arg(short = 'n', long = "max-count", default_value_t = DEFAULT_LOG_COUNT)]
        max_count: usize,
    },

    /// Preview or restore a previously recorded recovery view.
    #[cfg(unix)]
    Restore {
        /// Full 64-character recovery view identifier.
        #[arg(long)]
        view: ContentId,

        /// Preview changes without modifying Git state or recovery history.
        #[arg(long)]
        dry_run: bool,
    },

    /// Move HEAD (or the current branch), optionally resetting index and worktree paths.
    #[cfg(unix)]
    Reset {
        /// Move only HEAD or the current branch without changing index or working tree.
        #[arg(long, group = "mode")]
        soft: bool,

        /// Reset HEAD (or its branch) and the index without changing working tree (default).
        #[arg(long, group = "mode")]
        mixed: bool,

        /// Reset HEAD (or its branch), index, and tracked working tree paths.
        #[arg(long, group = "mode")]
        hard: bool,

        /// Commit target (`HEAD`, local branch, full commit ID, and optional `~N`).
        #[arg(default_value = "HEAD")]
        commit: String,
    },

    /// Undo the latest eligible completed git2 mutation (or undo an undo to redo).
    #[cfg(unix)]
    Undo {
        /// Preview changes without modifying Git state or recovery history.
        #[arg(long)]
        dry_run: bool,
    },

    /// Inspect, reconcile, or roll back an interrupted pending transaction.
    #[cfg(unix)]
    Recover {
        /// Inspect pending transaction status without modifying anything.
        #[arg(long, conflicts_with = "rollback")]
        status: bool,

        /// Roll back an un-selected pending transaction to its pre-operation view.
        #[arg(long, conflicts_with = "status")]
        rollback: bool,
    },

    /// Show the immutable local operation history, newest first.
    #[cfg(unix)]
    Oplog,

    /// Show changes between HEAD, the index, and the working tree.
    #[cfg(unix)]
    Status {
        /// Terminate records with NUL for byte-safe scripting.
        #[arg(short = 'z', long)]
        nul: bool,
    },

    /// Manage configured remote repositories (`list`, `add`, `remove`, `rename`, `get-url`, `set-url`).
    #[cfg(unix)]
    Remote(RemoteArgs),

    /// Clone a repository and adopt it into local `.git/git2/` recovery storage.
    #[cfg(unix)]
    Clone {
        /// Check out this branch instead of the remote's HEAD branch.
        #[arg(short = 'b', long)]
        branch: Option<String>,

        /// Name to assign the upstream remote instead of `origin`.
        #[arg(short = 'o', long, value_name = "NAME")]
        origin: Option<String>,

        /// Skip automatic `.git/git2/` recovery adoption after cloning.
        #[arg(long)]
        no_adopt: bool,

        /// Refuse eligible files larger than this many MiB when auto-adopting.
        #[arg(long, default_value_t = 100)]
        max_file_mib: u64,

        /// Source repository URL or path.
        #[arg(value_name = "URL")]
        url: String,

        /// Destination working directory.
        #[arg(value_name = "DIRECTORY")]
        directory: Option<PathBuf>,
    },

    /// Fetch objects and refs from a remote into an adopted repository.
    #[cfg(unix)]
    Fetch {
        /// Fetch from all configured remotes.
        #[arg(long)]
        all: bool,

        /// Prune deleted remote-tracking branches.
        #[arg(short = 'p', long)]
        prune: bool,

        /// Fetch all tags from the remote.
        #[arg(short = 't', long)]
        tags: bool,

        /// Remote name or URL to fetch from.
        #[arg(value_name = "REMOTE")]
        remote: Option<String>,

        /// Explicit refspecs to fetch.
        #[arg(value_name = "REFSPEC")]
        refspecs: Vec<String>,
    },

    /// Fetch and update the current branch (`--ff-only` by default; `--merge`/`--rebase` are conflict-recoverable).
    #[cfg(unix)]
    Pull(PullArgs),

    /// Push local refs to a remote repository (not undoable by local `git2 undo`).
    #[cfg(unix)]
    Push(PushArgs),

    /// Inspect the object layer while the user-facing commands are built.
    Debug(DebugArgs),
}

#[cfg(unix)]
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
struct PullArgs {
    /// Only allow fast-forward updates (default).
    #[arg(long, group = "pull_mode")]
    ff_only: bool,

    /// Allow non-fast-forward merge commits (conflict-recoverable).
    #[arg(long, group = "pull_mode")]
    merge: bool,

    /// Rebase local commits onto upstream (conflict-recoverable).
    #[arg(long, group = "pull_mode")]
    rebase: bool,

    /// Prune deleted remote-tracking branches during fetch.
    #[arg(short = 'p', long)]
    prune: bool,

    /// Remote name or URL to pull from.
    #[arg(value_name = "REMOTE")]
    remote: Option<String>,

    /// Explicit refspecs to pull.
    #[arg(value_name = "REFSPEC")]
    refspecs: Vec<String>,
}

#[cfg(unix)]
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
struct PushArgs {
    /// Set upstream tracking reference for pushed branches.
    #[arg(short = 'u', long)]
    set_upstream: bool,

    /// Push all local tags.
    #[arg(long)]
    tags: bool,

    /// Delete the named remote refs.
    #[arg(short = 'd', long)]
    delete: bool,

    /// Protect remote branch tips with `--force-with-lease`.
    #[arg(long, conflicts_with = "force")]
    force_with_lease: bool,

    /// Force-update remote refs without lease protection.
    #[arg(short = 'f', long, conflicts_with = "force_with_lease")]
    force: bool,

    /// Preview push updates without sending refs to the remote.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Remote name or URL to push to.
    #[arg(value_name = "REMOTE")]
    remote: Option<String>,

    /// Explicit refspecs to push.
    #[arg(value_name = "REFSPEC")]
    refspecs: Vec<String>,
}

#[cfg(unix)]
#[derive(Args)]
struct RemoteArgs {
    /// Show URLs alongside remote names when listing remotes.
    #[arg(short = 'v', long)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<RemoteCommand>,
}

#[cfg(unix)]
#[derive(Subcommand)]
enum RemoteCommand {
    /// Add a new remote repository.
    Add { name: String, url: String },
    /// Remove a configured remote repository.
    Remove { name: String },
    /// Rename a configured remote repository.
    Rename { old: String, new: String },
    /// Print configured URLs for a remote repository.
    GetUrl {
        #[arg(long)]
        push: bool,
        #[arg(long)]
        all: bool,
        name: String,
    },
    /// Change the URL of an existing remote repository.
    SetUrl {
        #[arg(long)]
        push: bool,
        name: String,
        new_url: String,
        old_url: Option<String>,
    },
}

#[derive(Args)]
struct DebugArgs {
    #[command(subcommand)]
    command: DebugCommand,
}

#[derive(Subcommand)]
enum DebugCommand {
    /// Hash a file as a blob without modifying the repository.
    HashObject {
        /// Read object data from standard input.
        #[arg(long, conflicts_with = "path")]
        stdin: bool,

        /// File whose bytes should be hashed.
        path: Option<PathBuf>,
    },

    /// Print an object from the current repository.
    CatObject {
        /// Print only the object's type.
        #[arg(short = 't', long)]
        type_only: bool,

        /// Full 40-character object identifier.
        oid: GitOid,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        #[cfg(unix)]
        Command::Adopt { max_file_mib } => run_adopt(max_file_mib),
        #[cfg(unix)]
        Command::Checkpoint => run_checkpoint(),
        #[cfg(unix)]
        Command::Add { all, paths } => run_add(all, &paths),
        #[cfg(unix)]
        Command::Commit { message } => run_commit(&message),
        #[cfg(unix)]
        Command::Log { max_count } => run_log(max_count),
        #[cfg(unix)]
        Command::Restore { view, dry_run } => run_restore(view, dry_run),
        #[cfg(unix)]
        Command::Reset {
            soft,
            mixed,
            hard,
            commit,
        } => run_reset(soft, mixed, hard, &commit),
        #[cfg(unix)]
        Command::Undo { dry_run } => run_undo(dry_run),
        #[cfg(unix)]
        Command::Recover { status, rollback } => run_recover(status, rollback),
        #[cfg(unix)]
        Command::Oplog => run_oplog(),
        #[cfg(unix)]
        Command::Status { nul } => run_status(nul),
        #[cfg(unix)]
        Command::Remote(args) => run_remote(args),
        #[cfg(unix)]
        Command::Clone {
            branch,
            origin,
            no_adopt,
            max_file_mib,
            url,
            directory,
        } => run_clone(branch, origin, no_adopt, max_file_mib, url, directory),
        #[cfg(unix)]
        Command::Fetch {
            all,
            prune,
            tags,
            remote,
            refspecs,
        } => run_fetch(all, prune, tags, remote, refspecs),
        #[cfg(unix)]
        Command::Pull(args) => run_pull(args),
        #[cfg(unix)]
        Command::Push(args) => run_push(args),
        Command::Debug(args) => run_debug(args.command),
    }
}

#[cfg(unix)]
fn run_adopt(max_file_mib: u64) -> anyhow::Result<()> {
    let max_file_bytes = max_file_mib
        .checked_mul(1024 * 1024)
        .context("--max-file-mib is too large")?;
    let repository =
        LocalRepository::discover(".").context("repository cannot safely be adopted")?;
    let adoption = repository
        .adopt(AdoptOptions { max_file_bytes })
        .context("could not capture the recovery baseline")?;

    println!(
        "adopted {} files ({} bytes) as view {}",
        adoption.file_count, adoption.content_bytes, adoption.view_id
    );
    if adoption.content_bytes >= WARN_STORE_BYTES {
        eprintln!(
            "warning: the baseline contains at least 5 GiB; git2 will not prune recovery data silently"
        );
    }
    Ok(())
}

#[cfg(unix)]
fn run_checkpoint() -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let imported = repository
        .checkpoint()
        .context("could not save a checkpoint")?;
    println!(
        "checkpointed {} files ({} bytes) as view {}",
        imported.file_count, imported.content_bytes, imported.view_id
    );
    Ok(())
}

#[cfg(unix)]
fn run_status(nul: bool) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    if RecoveryStore::exists(repository.git_dir()).context("could not check for recovery data")? {
        repository
            .import_external_changes()
            .context("could not import changes made outside git2")?;
    }
    let status = repository.status().context("could not calculate status")?;
    io::stdout()
        .lock()
        .write_all(&status.porcelain_v1(nul))
        .context("could not write status to standard output")
}

#[cfg(unix)]
fn ensure_no_pending() -> anyhow::Result<()> {
    if let Ok(recovery) = RecoveryRepository::open(".")
        && let Some(summary) =
            transaction::pending(&recovery).context("could not inspect pending transaction")?
    {
        bail!(
            "a pending transaction {} ({}) exists for {}; run `git2 recover --status`, `git2 recover`, or `git2 recover --rollback` first",
            summary.tx_id,
            phase_label(summary.phase),
            String::from_utf8_lossy(summary.command.as_ref()),
        );
    }
    Ok(())
}

#[cfg(unix)]
fn print_preview_changes(changes: &[Change]) {
    if changes.is_empty() {
        println!("no changes");
    } else {
        for change in changes {
            println!("{}", format_change_line(change));
        }
    }
}

#[cfg(unix)]
fn run_restore(view: ContentId, dry_run: bool) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    if dry_run {
        let plan = restore::plan(&repository, view)
            .with_context(|| format!("could not plan restore to view {view}"))?;
        print_preview_changes(plan.changes());
        return Ok(());
    }

    let operation_id = run_restore_apply(&repository, view)
        .with_context(|| format!("could not restore view {view}"))?;
    println!("restored view {view} as operation {operation_id}");
    Ok(())
}

#[cfg(unix)]
fn run_reset(soft: bool, _mixed: bool, hard: bool, commit: &str) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let mode = if soft {
        ResetMode::Soft
    } else if hard {
        ResetMode::Hard
    } else {
        ResetMode::Mixed
    };
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let outcome = run_reset_apply(&repository, mode, commit)
        .with_context(|| format!("could not reset ({}) to {commit}", mode.as_str()))?;
    println!(
        "reset ({}) to {} at view {} as operation {}",
        mode.as_str(),
        outcome.target_commit,
        outcome.after_view,
        outcome.operation_id
    );
    Ok(())
}

#[cfg(unix)]
fn run_add(all: bool, paths: &[OsString]) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let cwd = std::env::current_dir().context("could not determine current working directory")?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let outcome =
        run_add_apply(&repository, &cwd, all, paths).context("could not stage changes")?;
    println!(
        "staged {} entries (removed {}) at view {} as operation {}",
        outcome.staged_count, outcome.removed_count, outcome.after_view, outcome.operation_id
    );
    Ok(())
}

#[cfg(unix)]
fn run_commit(message: &str) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let outcome = run_commit_apply(&repository, message).context("could not create commit")?;
    println!(
        "committed {} (tree {}) at view {} as operation {}",
        outcome.commit_oid, outcome.tree_oid, outcome.after_view, outcome.operation_id
    );
    Ok(())
}

#[cfg(unix)]
fn run_log(max_count: usize) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let entries = log::walk(&repository, LogOptions { max_count })
        .context("could not walk commit history")?;
    let mut stdout = io::stdout().lock();
    for entry in entries {
        stdout
            .write_all(&entry.format_text())
            .context("could not write commit log to standard output")?;
    }
    Ok(())
}

#[cfg(unix)]
fn run_undo(dry_run: bool) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    if dry_run {
        let plan = undo::plan(&repository).context("could not plan undo")?;
        print_preview_changes(plan.changes());
        return Ok(());
    }
    let outcome = run_undo_apply(&repository).context("could not undo latest operation")?;
    println!(
        "undid operation to view {} as operation {}",
        outcome.restored_view, outcome.operation_id
    );
    Ok(())
}

#[cfg(all(unix, debug_assertions))]
fn run_add_apply(
    repository: &LocalRepository,
    cwd: &std::path::Path,
    all: bool,
    paths: &[OsString],
) -> git2_core::Result<AddOutcome> {
    stage::apply_with_hook(repository, cwd, all, paths, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_add_apply(
    repository: &LocalRepository,
    cwd: &std::path::Path,
    all: bool,
    paths: &[OsString],
) -> git2_core::Result<AddOutcome> {
    stage::apply(repository, cwd, all, paths)
}

#[cfg(all(unix, debug_assertions))]
fn run_commit_apply(
    repository: &LocalRepository,
    message: &str,
) -> git2_core::Result<CommitOutcome> {
    commit::apply_with_hook(repository, message, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_commit_apply(
    repository: &LocalRepository,
    message: &str,
) -> git2_core::Result<CommitOutcome> {
    commit::apply(repository, message)
}

#[cfg(all(unix, debug_assertions))]
fn run_restore_apply(
    repository: &LocalRepository,
    view: ContentId,
) -> git2_core::Result<ContentId> {
    restore::apply_with_hook(repository, view, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_restore_apply(
    repository: &LocalRepository,
    view: ContentId,
) -> git2_core::Result<ContentId> {
    restore::apply(repository, view)
}

#[cfg(all(unix, debug_assertions))]
fn run_reset_apply(
    repository: &LocalRepository,
    mode: ResetMode,
    commit: &str,
) -> git2_core::Result<ResetOutcome> {
    reset::apply_with_hook(repository, mode, commit, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_reset_apply(
    repository: &LocalRepository,
    mode: ResetMode,
    commit: &str,
) -> git2_core::Result<ResetOutcome> {
    reset::apply(repository, mode, commit)
}

#[cfg(all(unix, debug_assertions))]
fn run_undo_apply(repository: &LocalRepository) -> git2_core::Result<UndoOutcome> {
    undo::apply_with_hook(repository, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_undo_apply(repository: &LocalRepository) -> git2_core::Result<UndoOutcome> {
    undo::apply(repository)
}

#[cfg(unix)]
fn run_recover(status: bool, rollback: bool) -> anyhow::Result<()> {
    let recovery = RecoveryRepository::open(".").context("repository is not adopted")?;
    if status {
        let Some(summary) =
            transaction::pending(&recovery).context("could not inspect pending transaction")?
        else {
            println!("no pending transaction");
            return Ok(());
        };
        println!("pending transaction {}", summary.tx_id);
        println!("phase: {}", phase_label(summary.phase));
        println!(
            "command: {}",
            String::from_utf8_lossy(summary.command.as_ref())
        );
        println!("before view: {}", summary.before_view);
        println!("target view: {}", summary.target_view);
        println!(
            "progress: {}/{}",
            summary.next_action, summary.total_actions
        );
        println!("operation: {}", summary.operation_id);
        if summary.conflict_state {
            println!(
                "conflict state: present (resolve in Git and run `git2 recover`, or abort with `git2 recover --rollback`)"
            );
        }
        return Ok(());
    }

    let outcome = if rollback {
        transaction::rollback(&recovery).context("could not roll back pending transaction")?
    } else {
        run_transaction_recover(&recovery).context("could not recover pending transaction")?
    };
    match outcome {
        RecoveryOutcome::Clean => println!("no pending transaction"),
        RecoveryOutcome::Aborted { before_view } => {
            println!("aborted unapplied transaction; pre-operation view {before_view} intact");
        }
        RecoveryOutcome::RolledBack { before_view } => {
            println!("rolled back pending transaction to pre-operation view {before_view}");
        }
        RecoveryOutcome::Finalized {
            operation_id,
            after_view,
        } => {
            println!("recovered operation {operation_id} at view {after_view}");
        }
    }
    Ok(())
}

#[cfg(all(unix, debug_assertions))]
fn run_transaction_recover(recovery: &RecoveryRepository) -> git2_core::Result<RecoveryOutcome> {
    transaction::recover_with_hook(recovery, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_transaction_recover(recovery: &RecoveryRepository) -> git2_core::Result<RecoveryOutcome> {
    transaction::recover(recovery)
}

#[cfg(unix)]
const fn phase_label(phase: PendingPhase) -> &'static str {
    match phase {
        PendingPhase::Prepared => "prepared",
        PendingPhase::Applying => "applying",
        PendingPhase::Finalizing => "finalizing",
        PendingPhase::Committed => "committed",
        PendingPhase::Delegated => "delegated",
    }
}

#[cfg(unix)]
fn run_remote(args: RemoteArgs) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let action = match args.command {
        None => RemoteAction::List {
            verbose: args.verbose,
        },
        Some(RemoteCommand::Add { name, url }) => RemoteAction::Add { name, url },
        Some(RemoteCommand::Remove { name }) => RemoteAction::Remove { name },
        Some(RemoteCommand::Rename { old, new }) => RemoteAction::Rename { old, new },
        Some(RemoteCommand::GetUrl { push, all, name }) => RemoteAction::GetUrl { name, push, all },
        Some(RemoteCommand::SetUrl {
            push,
            name,
            new_url,
            old_url,
        }) => RemoteAction::SetUrl {
            name,
            new_url,
            old_url,
            push,
        },
    };
    let output = run_remote_apply(&repository, &action).context("remote command failed")?;
    if !output.stdout.is_empty() {
        io::stdout()
            .lock()
            .write_all(&output.stdout)
            .context("could not write remote output to standard output")?;
    }
    Ok(())
}

#[cfg(unix)]
fn run_clone(
    branch: Option<String>,
    origin: Option<String>,
    no_adopt: bool,
    max_file_mib: u64,
    url: String,
    directory: Option<PathBuf>,
) -> anyhow::Result<()> {
    let max_file_bytes = max_file_mib
        .checked_mul(1024 * 1024)
        .context("--max-file-mib is too large")?;
    let cwd = std::env::current_dir().context("could not determine current working directory")?;
    let outcome = network::clone_repo(
        &cwd,
        &CloneOptions {
            url,
            directory,
            branch,
            origin,
            adopt: !no_adopt,
            max_file_bytes,
        },
    )
    .context("could not clone repository")?;
    if let Some(adoption) = outcome.adoption {
        println!(
            "cloned {} and adopted {} files ({} bytes) as view {}",
            outcome.workdir.display(),
            adoption.file_count,
            adoption.content_bytes,
            adoption.view_id
        );
    } else {
        println!("cloned {} without adoption", outcome.workdir.display());
    }
    Ok(())
}

#[cfg(unix)]
fn run_fetch(
    all: bool,
    prune: bool,
    tags: bool,
    remote: Option<String>,
    refspecs: Vec<String>,
) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let outcome = run_fetch_apply(
        &repository,
        &FetchOptions {
            remote,
            refspecs,
            all,
            prune,
            tags,
        },
    )
    .context("could not fetch from remote")?;
    if let Some(op_id) = outcome.operation_id {
        println!(
            "fetched at view {} as operation {}",
            outcome.after_view, op_id
        );
    } else {
        println!(
            "fetch complete; already up to date at view {}",
            outcome.after_view
        );
    }
    Ok(())
}

#[cfg(unix)]
fn run_pull(args: PullArgs) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let mode = if args.merge {
        PullMode::Merge
    } else if args.rebase {
        PullMode::Rebase
    } else {
        PullMode::FfOnly
    };
    let _ = args.ff_only;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    let outcome = run_pull_apply(
        &repository,
        &PullOptions {
            mode,
            prune: args.prune,
            remote: args.remote,
            refspecs: args.refspecs,
        },
    )
    .context("could not pull from remote")?;
    if let Some(op_id) = outcome.operation_id {
        println!(
            "pulled at view {} as operation {}",
            outcome.after_view, op_id
        );
    } else {
        println!("already up to date at view {}", outcome.after_view);
    }
    Ok(())
}

#[cfg(unix)]
fn run_push(args: PushArgs) -> anyhow::Result<()> {
    ensure_no_pending()?;
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    if !args.dry_run {
        if args.force {
            eprintln!(
                "warning: force-pushing rewrites remote history and cannot be undone by local `git2 undo`"
            );
        } else {
            eprintln!("note: remote pushes cannot be undone by local `git2 undo`");
        }
    }
    let outcome = run_push_apply(
        &repository,
        &PushOptions {
            remote: args.remote,
            refspecs: args.refspecs,
            set_upstream: args.set_upstream,
            tags: args.tags,
            delete: args.delete,
            force_with_lease: args.force_with_lease,
            force: args.force,
            dry_run: args.dry_run,
        },
    )
    .context("could not push to remote")?;
    if let (Some(op_id), Some(after_view)) = (outcome.operation_id, outcome.after_view) {
        println!("pushed at view {after_view} as operation {op_id}");
    }
    Ok(())
}

#[cfg(all(unix, debug_assertions))]
fn run_remote_apply(
    repository: &LocalRepository,
    action: &RemoteAction,
) -> git2_core::Result<RemoteOutput> {
    remote::run_with_hook(repository, action, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_remote_apply(
    repository: &LocalRepository,
    action: &RemoteAction,
) -> git2_core::Result<RemoteOutput> {
    remote::run(repository, action)
}

#[cfg(all(unix, debug_assertions))]
fn run_fetch_apply(
    repository: &LocalRepository,
    options: &FetchOptions,
) -> git2_core::Result<FetchOutcome> {
    network::fetch_with_hook(repository, options, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_fetch_apply(
    repository: &LocalRepository,
    options: &FetchOptions,
) -> git2_core::Result<FetchOutcome> {
    network::fetch(repository, options)
}

#[cfg(all(unix, debug_assertions))]
fn run_pull_apply(
    repository: &LocalRepository,
    options: &PullOptions,
) -> git2_core::Result<PullOutcome> {
    network::pull_with_hook(repository, options, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_pull_apply(
    repository: &LocalRepository,
    options: &PullOptions,
) -> git2_core::Result<PullOutcome> {
    network::pull(repository, options)
}

#[cfg(all(unix, debug_assertions))]
fn run_push_apply(
    repository: &LocalRepository,
    options: &PushOptions,
) -> git2_core::Result<PushOutcome> {
    network::push_with_hook(repository, options, test_failpoint_hook())
}

#[cfg(all(unix, not(debug_assertions)))]
fn run_push_apply(
    repository: &LocalRepository,
    options: &PushOptions,
) -> git2_core::Result<PushOutcome> {
    network::push(repository, options)
}

#[cfg(unix)]
fn run_oplog() -> anyhow::Result<()> {
    let recovery = RecoveryRepository::open(".").context("repository is not adopted")?;
    let store = if let Some(summary) =
        transaction::pending(&recovery).context("could not inspect pending transaction")?
    {
        eprintln!(
            "warning: pending transaction {} ({}); showing last durable history",
            summary.tx_id,
            phase_label(summary.phase)
        );
        recovery.store().clone()
    } else {
        let repository = LocalRepository::discover(".")
            .context("could not open this supported Git repository")?;
        repository
            .import_external_changes()
            .context("could not import changes made outside git2")?;
        RecoveryStore::open(repository.git_dir()).context("repository is not adopted")?
    };
    let mut id = Some(store.head().context("recovery history has no HEAD")?);
    while let Some(operation_id) = id {
        let operation = store
            .read_operation(operation_id)
            .with_context(|| format!("could not read operation {operation_id}"))?;
        let kind = match &operation.kind {
            OperationKind::Adopt => "adopt".to_owned(),
            OperationKind::ExternalGit => "external-git".to_owned(),
            OperationKind::Git2(command) => {
                format!("git2:{}", String::from_utf8_lossy(command.as_ref()))
            }
        };
        println!(
            "{operation_id} {} {kind} {} -> {}",
            operation.timestamp_seconds, operation.before, operation.after
        );
        id = operation.parent;
    }
    Ok(())
}

#[cfg(all(unix, debug_assertions))]
fn test_failpoint_hook() -> impl FnMut(Failpoint) -> git2_core::Result<()> {
    let spec = std::env::var("GIT2_TEST_FAILPOINT").ok();
    move |fp| {
        let Some(raw) = spec.as_deref() else {
            return Ok(());
        };
        let (mode, tag) = if let Some(tag) = raw.strip_prefix("abort:") {
            ("abort", tag)
        } else if let Some(tag) = raw.strip_prefix("io:") {
            ("io", tag)
        } else {
            return Ok(());
        };
        if failpoint_matches(tag, fp) {
            match mode {
                "abort" => std::process::abort(),
                "io" => {
                    return Err(Error::Io(io::Error::other(format!(
                        "injected failpoint {tag}"
                    ))));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(all(unix, debug_assertions))]
fn failpoint_matches(tag: &str, fp: Failpoint) -> bool {
    match fp {
        Failpoint::BeforePendingWrite => tag == "before_pending_write",
        Failpoint::AfterPrepare => tag == "after_prepare",
        Failpoint::BeforeLockPublish(idx) => {
            tag.strip_prefix("before_lock_publish:") == Some(idx.to_string().as_str())
        }
        Failpoint::AfterLockPublish(idx) => {
            tag.strip_prefix("after_lock_publish:") == Some(idx.to_string().as_str())
        }
        Failpoint::BeforeAction(idx) => {
            tag.strip_prefix("before_action:") == Some(idx.to_string().as_str())
        }
        Failpoint::AfterAction(idx) => {
            tag.strip_prefix("after_action:") == Some(idx.to_string().as_str())
        }
        Failpoint::BeforeFinalize => tag == "before_finalize",
        Failpoint::AfterOperationWrite => tag == "after_operation_write",
        Failpoint::AfterHeadSelection => tag == "after_head_selection",
        Failpoint::BeforePendingCleanup => tag == "before_pending_cleanup",
        Failpoint::RecoverAfterOperationWrite => tag == "recover_after_operation_write",
        Failpoint::RecoverAfterHeadSelection => tag == "recover_after_head_selection",
    }
}

fn run_debug(command: DebugCommand) -> anyhow::Result<()> {
    match command {
        DebugCommand::HashObject { stdin, path } => {
            let data = read_input(stdin, path)?;
            let oid = hash_object(ObjectKind::Blob, &data);
            println!("{oid}");
        }
        DebugCommand::CatObject { type_only, oid } => {
            let object = ObjectStore::discover(".")
                .context("could not discover a Git repository")?
                .read(oid)
                .with_context(|| format!("could not read Git object {oid}"))?;
            if type_only {
                println!("{}", object.kind);
            } else {
                io::stdout()
                    .lock()
                    .write_all(&object.data)
                    .context("could not write object data to standard output")?;
            }
        }
    }
    Ok(())
}

fn read_input(stdin: bool, path: Option<PathBuf>) -> anyhow::Result<Vec<u8>> {
    if stdin {
        let mut data = Vec::new();
        io::stdin()
            .lock()
            .read_to_end(&mut data)
            .context("could not read standard input")?;
        return Ok(data);
    }

    let Some(path) = path else {
        bail!("provide a file path or pass --stdin");
    };
    fs::read(&path).with_context(|| format!("could not read {}", path.display()))
}
