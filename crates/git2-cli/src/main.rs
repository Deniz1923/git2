//! Command-line entry point for the experimental git2 client.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use git2_core::object::{GitOid, ObjectKind, hash_object};
#[cfg(unix)]
use git2_core::recovery::{ContentId, OperationKind, RecoveryStore};
#[cfg(unix)]
use git2_core::repository::{AdoptOptions, LocalRepository};
#[cfg(unix)]
use git2_core::reset::{self, ResetMode, ResetOutcome};
#[cfg(unix)]
use git2_core::restore;
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

    /// Inspect the object layer while the user-facing commands are built.
    Debug(DebugArgs),
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
    }
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
