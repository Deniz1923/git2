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
    for (marker, label) in [
        (b"1\n".as_slice(), "format 1"),
        (b"2\n".as_slice(), "format 2"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "--quiet"]);
        let root = directory.path().join(".git/git2");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("format"), marker).unwrap();
        std::fs::write(root.join("HEAD"), b"old history").unwrap();

        for command in ["adopt", "checkpoint", "status", "oplog"] {
            let output = git2(directory.path(), &[command]);
            assert!(!output.status.success(), "{command} must refuse {label}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains(label), "{command}: {error}");
            assert_eq!(std::fs::read(root.join("format")).unwrap(), marker);
            assert_eq!(std::fs::read(root.join("HEAD")).unwrap(), b"old history");
            assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        }
    }
}

fn assert_full_view_id(id: &str) {
    assert_eq!(id.len(), 64, "view id must be 64 characters: {id}");
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "view id must be lowercase hexadecimal: {id}"
    );
}

#[test]
fn checkpoint_refuses_unadopted_repository_and_saves_reusable_views() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    let work = directory.path().join("work.txt");
    std::fs::write(&work, b"before checkpoint\n").unwrap();

    let refused = git2(directory.path(), &["checkpoint"]);
    assert!(
        !refused.status.success(),
        "checkpoint must be refused before adoption"
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("git2 adopt"));
    assert!(
        !directory.path().join(".git/git2").exists(),
        "a refused checkpoint must not create a recovery store"
    );

    let adopted = git2(directory.path(), &["adopt"]);
    assert!(
        adopted.status.success(),
        "git2 adopt failed:\n{}",
        String::from_utf8_lossy(&adopted.stderr)
    );

    std::fs::write(&work, b"explicit checkpoint\n").unwrap();
    let checkpoint = git2(directory.path(), &["checkpoint"]);
    assert!(
        checkpoint.status.success(),
        "git2 checkpoint failed:\n{}",
        String::from_utf8_lossy(&checkpoint.stderr)
    );
    let stdout = String::from_utf8(checkpoint.stdout).unwrap();
    let (_, id) = stdout
        .trim_end()
        .rsplit_once(" as view ")
        .unwrap_or_else(|| panic!("checkpoint output must end with 'as view <id>': {stdout:?}"));
    assert_full_view_id(id);
    assert!(
        directory
            .path()
            .join(".git/git2/views")
            .join(&id[..2])
            .join(&id[2..])
            .is_file(),
        "checkpoint ID must name a stored view"
    );

    let history = || {
        let output = git2(directory.path(), &["oplog"]);
        assert!(
            output.status.success(),
            "git2 oplog failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let after_save = history();
    assert_eq!(
        after_save
            .lines()
            .filter(|line| line.contains("git2:checkpoint"))
            .count(),
        1,
        "a changed explicit save must append exactly one checkpoint operation:\n{after_save}"
    );
    let newest = after_save.lines().next().unwrap();
    assert!(
        newest.contains("git2:checkpoint"),
        "the newest operation must be the explicit checkpoint:\n{after_save}"
    );
    assert_eq!(
        newest.split_whitespace().last(),
        Some(id),
        "the full 64-char view id must appear in oplog:\n{after_save}"
    );

    let repeated = git2(directory.path(), &["checkpoint"]);
    assert!(
        repeated.status.success(),
        "an unchanged checkpoint must succeed:\n{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    let repeated_stdout = String::from_utf8(repeated.stdout).unwrap();
    let (_, repeated_id) = repeated_stdout
        .trim_end()
        .rsplit_once(" as view ")
        .unwrap_or_else(|| {
            panic!("checkpoint output must end with 'as view <id>': {repeated_stdout:?}")
        });
    assert_eq!(
        repeated_id, id,
        "an unchanged checkpoint must reuse the same view id"
    );
    assert_eq!(
        history(),
        after_save,
        "an unchanged checkpoint must not append another operation"
    );
}

#[test]
fn debug_hash_object_write_is_rejected_without_creating_an_object() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    std::fs::write(directory.path().join("input"), b"never written\0").unwrap();

    let canonical = git(directory.path(), &["hash-object", "input"]);
    let oid = String::from_utf8(canonical.stdout).unwrap();
    let oid = oid.trim();
    let objects = directory.path().join(".git").join("objects");
    let object_path = objects.join(&oid[..2]).join(&oid[2..]);
    assert!(!object_path.exists(), "the object must start absent");
    let before = std::fs::read_dir(&objects).unwrap().count();

    let rejected = git2(
        directory.path(),
        &["debug", "hash-object", "--write", "input"],
    );
    assert!(
        !rejected.status.success(),
        "debug hash-object --write must be rejected"
    );
    assert!(
        rejected.stdout.is_empty(),
        "a rejected write must not print an object id"
    );
    assert!(
        !rejected.stderr.is_empty(),
        "the rejection must explain why --write is not allowed"
    );
    assert!(
        !object_path.exists(),
        "--write must not create a Git object"
    );
    assert_eq!(
        std::fs::read_dir(&objects).unwrap().count(),
        before,
        "the object database must be unchanged"
    );
}

#[test]
fn restore_dry_run_apply_and_clean_recover_status() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet", "-b", "main"]);
    std::fs::write(directory.path().join("file.txt"), b"v1\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    git(
        directory.path(),
        &[
            "-c",
            "user.name=git2",
            "-c",
            "user.email=git2@example.invalid",
            "commit",
            "-q",
            "-m",
            "v1",
        ],
    );

    let adopt = git2(directory.path(), &["adopt"]);
    assert!(adopt.status.success());
    let adopt_stdout = String::from_utf8(adopt.stdout).unwrap();
    let (_, v1) = adopt_stdout.trim_end().rsplit_once(" as view ").unwrap();
    assert_full_view_id(v1);

    std::fs::write(directory.path().join("file.txt"), b"v2\n").unwrap();
    let cp = git2(directory.path(), &["checkpoint"]);
    assert!(cp.status.success());

    let dry = git2(directory.path(), &["restore", "--view", v1, "--dry-run"]);
    assert!(dry.status.success());
    let dry_stdout = String::from_utf8(dry.stdout).unwrap();
    assert!(dry_stdout.contains("replace worktree file.txt"));
    assert_eq!(
        std::fs::read(directory.path().join("file.txt")).unwrap(),
        b"v2\n"
    );

    let apply = git2(directory.path(), &["restore", "--view", v1]);
    assert!(apply.status.success());
    let apply_stdout = String::from_utf8(apply.stdout).unwrap();
    assert!(apply_stdout.contains(&format!("restored view {v1} as operation ")));
    assert_eq!(
        std::fs::read(directory.path().join("file.txt")).unwrap(),
        b"v1\n"
    );

    let rec_status = git2(directory.path(), &["recover", "--status"]);
    assert!(rec_status.status.success());
    assert_eq!(
        String::from_utf8(rec_status.stdout).unwrap().trim(),
        "no pending transaction"
    );
}

#[test]
fn mismatched_git_dir_and_core_worktree_is_refused() {
    let repo_a = tempfile::tempdir().unwrap();
    let repo_b = tempfile::tempdir().unwrap();
    for repo in [repo_a.path(), repo_b.path()] {
        git(repo, &["init", "--quiet", "-b", "main"]);
    }
    git(
        repo_b.path(),
        &["config", "core.worktree", repo_a.path().to_str().unwrap()],
    );

    let output = git2(repo_b.path(), &["status"]);
    assert!(
        !output.status.success(),
        "mismatched core.worktree must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(".git"),
        "expected .git directory ownership refusal, got: {stderr}"
    );
}
