#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use git2_core::Error;
use git2_core::recovery::RecoveryStore;
use git2_core::repository::{AdoptOptions, LocalRepository};

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .expect("Git must be installed for adoption tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary repository");
    git(directory.path(), &["init", "--quiet"]);
    directory
}

#[test]
fn adoption_captures_tracked_and_nonignored_untracked_files() {
    let directory = repository();
    std::fs::write(directory.path().join(".gitignore"), b"ignored.bin\n").unwrap();
    std::fs::write(
        directory.path().join("tracked.txt"),
        b"tracked before adoption\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("untracked.bin"), b"untracked\0bytes").unwrap();
    std::fs::write(
        directory.path().join("ignored.bin"),
        b"must not enter recovery",
    )
    .unwrap();
    git(directory.path(), &["add", ".gitignore", "tracked.txt"]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=git2 test",
            "-c",
            "user.email=git2@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    );

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let adoption = repository.adopt(AdoptOptions::default()).unwrap();
    assert_eq!(adoption.file_count, 3);

    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    assert_eq!(store.current_view().unwrap(), adoption.view_id);
    let view = store.read_view(adoption.view_id).unwrap();
    let paths = view
        .files
        .iter()
        .map(|file| {
            let path: &[u8] = file.path.as_ref();
            path.to_vec()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            b".gitignore".to_vec(),
            b"tracked.txt".to_vec(),
            b"untracked.bin".to_vec()
        ]
    );
    assert!(view.index.is_some());
    assert!(!view.refs.is_empty());
    assert!(
        !view.files.iter().any(|file| {
            let path: &[u8] = file.path.as_ref();
            path == b"ignored.bin"
        }),
        "ignored files are outside the recovery guarantee"
    );
    let untracked = view
        .files
        .iter()
        .find(|file| {
            let path: &[u8] = file.path.as_ref();
            path == b"untracked.bin"
        })
        .unwrap();
    assert_eq!(
        store.read_content(untracked.content).unwrap(),
        b"untracked\0bytes"
    );
    assert!(
        git(directory.path(), &["status", "--porcelain"])
            .stderr
            .is_empty()
    );
}

#[test]
fn oversize_file_refuses_adoption_before_creating_sidecar() {
    let directory = repository();
    std::fs::write(directory.path().join("large.bin"), b"12345").unwrap();
    let repository = LocalRepository::discover(directory.path()).unwrap();
    let error = repository
        .adopt(AdoptOptions { max_file_bytes: 4 })
        .unwrap_err();
    assert!(matches!(error, Error::UnsupportedRepository(_)));
    assert!(!repository.git_dir().join("git2").exists());
}

#[test]
fn in_progress_git_operation_is_refused() {
    let directory = repository();
    std::fs::write(
        directory.path().join(".git").join("MERGE_HEAD"),
        b"fixture\n",
    )
    .unwrap();
    let Err(error) = LocalRepository::discover(directory.path()) else {
        panic!("in-progress merge must be refused");
    };
    assert!(matches!(error, Error::UnsupportedRepository(_)));
}

#[test]
fn external_git_changes_append_one_operation() {
    let directory = repository();
    std::fs::write(directory.path().join("tracked"), b"one\n").unwrap();
    git(directory.path(), &["add", "tracked"]);

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let baseline = repository.adopt(AdoptOptions::default()).unwrap();
    std::fs::write(directory.path().join("tracked"), b"two\n").unwrap();
    std::fs::write(directory.path().join("new"), b"new\n").unwrap();

    let imported = repository
        .import_external_changes(AdoptOptions::default())
        .unwrap();
    assert!(imported.changed);
    assert_ne!(imported.view_id, baseline.view_id);
    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    let operation = store.read_operation(imported.operation_id).unwrap();
    assert_eq!(operation.parent, Some(baseline.operation_id));
    assert_eq!(operation.before, baseline.view_id);
    assert_eq!(operation.after, imported.view_id);

    let unchanged = repository
        .import_external_changes(AdoptOptions::default())
        .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(unchanged.operation_id, imported.operation_id);
}
