//! Command-line entry point for the experimental git2 client.

use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};
use git2_core::object::{GitOid, ObjectKind, RawObject, hash_object};
#[cfg(unix)]
use git2_core::recovery::{OperationKind, RecoveryStore};
#[cfg(unix)]
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::store::ObjectStore;

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
    /// Hash a file as a blob, optionally writing it to the current repository.
    HashObject {
        /// Write the object into the discovered repository.
        #[arg(short = 'w', long)]
        write: bool,

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
fn run_oplog() -> anyhow::Result<()> {
    let repository =
        LocalRepository::discover(".").context("could not open this supported Git repository")?;
    repository
        .import_external_changes()
        .context("could not import changes made outside git2")?;
    let store = RecoveryStore::open(repository.git_dir()).context("repository is not adopted")?;
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
            "{} {} {} {} -> {}",
            &operation_id.to_hex()[..12],
            operation.timestamp_seconds,
            kind,
            &operation.before.to_hex()[..12],
            &operation.after.to_hex()[..12]
        );
        id = operation.parent;
    }
    Ok(())
}

fn run_debug(command: DebugCommand) -> anyhow::Result<()> {
    match command {
        DebugCommand::HashObject { write, stdin, path } => {
            let data = read_input(stdin, path)?;
            let object = RawObject {
                kind: ObjectKind::Blob,
                data,
            };
            let oid = if write {
                ObjectStore::discover(".")
                    .context("could not discover a Git repository")?
                    .write(&object)
                    .context("could not write the Git object")?
            } else {
                hash_object(object.kind, &object.data)
            };
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
