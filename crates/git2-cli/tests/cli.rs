#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

fn git2(directory: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_git2"))
        .args(arguments)
        .current_dir(directory)
        .output()
        .expect("run git2")
}

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .output()
        .expect("Git must be installed for CLI tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn adopt_command_creates_one_baseline() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    std::fs::write(directory.path().join("work.txt"), b"recover me\n").unwrap();

    let output = git2(directory.path(), &["adopt"]);
    assert!(
        output.status.success(),
        "git2 adopt failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.starts_with(b"adopted 1 files"));
    assert!(directory.path().join(".git/git2/HEAD").is_file());

    std::fs::write(directory.path().join("work.txt"), b"changed outside git2\n").unwrap();
    let oplog = git2(directory.path(), &["oplog"]);
    assert!(oplog.status.success());
    let history = String::from_utf8(oplog.stdout).unwrap();
    assert_eq!(history.lines().count(), 2);
    assert!(history.contains("external-git"));
    assert!(history.contains("adopt"));

    let repeated = git2(directory.path(), &["adopt"]);
    assert!(!repeated.status.success());
    assert!(
        String::from_utf8_lossy(&repeated.stderr).contains("already has a git2 recovery store")
    );
}

#[test]
fn hash_object_command_matches_git() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input"), b"same as Git\0\n").unwrap();

    let ours = git2(directory.path(), &["debug", "hash-object", "input"]);
    assert!(ours.status.success());
    let canonical = git(directory.path(), &["hash-object", "input"]);
    assert_eq!(ours.stdout, canonical.stdout);
}

#[test]
fn adoption_and_auto_import_leave_legacy_store_untouched() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    let root = directory.path().join(".git/git2");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("format"), b"1\n").unwrap();
    std::fs::write(root.join("HEAD"), b"old history").unwrap();

    for command in ["adopt", "status", "oplog"] {
        let output = git2(directory.path(), &[command]);
        assert!(!output.status.success(), "{command} must refuse format 1");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("format 1"), "{command}: {error}");
        assert_eq!(std::fs::read(root.join("format")).unwrap(), b"1\n");
        assert_eq!(std::fs::read(root.join("HEAD")).unwrap(), b"old history");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    }
}
