#![cfg(all(unix, debug_assertions))]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git2(directory: &Path, arguments: &[&str], failpoint: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git2"));
    cmd.args(arguments).current_dir(directory);
    if let Some(fp) = failpoint {
        cmd.env("GIT2_TEST_FAILPOINT", fp);
    }
    cmd.output().expect("run git2")
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "git2 test")
        .env("GIT_AUTHOR_EMAIL", "git2@example.invalid")
        .env("GIT_COMMITTER_NAME", "git2 test")
        .env("GIT_COMMITTER_EMAIL", "git2@example.invalid")
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init_two_commit_repo() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    fs::write(dir.path().join("file.txt"), b"commit 1\n").unwrap();
    fs::write(dir.path().join("swap"), b"swap file\n").unwrap();
    git(dir.path(), &["add", "file.txt", "swap"]);
    git(dir.path(), &["commit", "-q", "-m", "c1"]);
    let c1 = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_owned();

    git(dir.path(), &["rm", "-q", "swap"]);
    fs::create_dir(dir.path().join("swap")).unwrap();
    fs::write(dir.path().join("swap/inner.txt"), b"swap dir\n").unwrap();
    fs::write(dir.path().join("file.txt"), b"commit 2\n").unwrap();
    fs::write(dir.path().join(".gitignore"), b"*.ignored\n").unwrap();
    git(
        dir.path(),
        &["add", "swap/inner.txt", "file.txt", ".gitignore"],
    );
    git(dir.path(), &["commit", "-q", "-m", "c2"]);
    let c2 = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_owned();

    fs::write(dir.path().join("note.ignored"), b"keep ignored\n").unwrap();
    fs::write(dir.path().join("untracked.txt"), b"keep untracked\n").unwrap();
    fs::write(dir.path().join("file.txt"), b"dirty staged\n").unwrap();
    git(dir.path(), &["add", "file.txt"]);
    fs::write(dir.path().join("file.txt"), b"dirty worktree\n").unwrap();

    let adopt = git2(dir.path(), &["adopt"], None);
    assert!(adopt.status.success());
    (dir, c1, c2)
}

#[test]
fn cli_demo_reset_hard_status_undo_redo_and_oplog() {
    let (dir, c1, c2) = init_two_commit_repo();
    let raw_index_before = fs::read(dir.path().join(".git/index")).unwrap();

    // Test soft and default mixed reset briefly via CLI first, then undo back
    let soft = git2(dir.path(), &["reset", "--soft", "HEAD~1"], None);
    assert!(
        soft.status.success(),
        "{}",
        String::from_utf8_lossy(&soft.stderr)
    );
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]).trim(), c1);
    assert_eq!(
        fs::read(dir.path().join("file.txt")).unwrap(),
        b"dirty worktree\n"
    );

    let undo_soft = git2(dir.path(), &["undo"], None);
    assert!(undo_soft.status.success());
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]).trim(), c2);
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        raw_index_before
    );

    // Hard reset across file/dir swap back to `HEAD~1`
    let hard = git2(dir.path(), &["reset", "--hard", "HEAD~1"], None);
    assert!(
        hard.status.success(),
        "{}",
        String::from_utf8_lossy(&hard.stderr)
    );
    assert_eq!(
        fs::read(dir.path().join("file.txt")).unwrap(),
        b"commit 1\n"
    );
    assert_eq!(fs::read(dir.path().join("swap")).unwrap(), b"swap file\n");
    assert_eq!(
        fs::read(dir.path().join("note.ignored")).unwrap(),
        b"keep ignored\n"
    );
    assert_eq!(
        fs::read(dir.path().join("untracked.txt")).unwrap(),
        b"keep untracked\n"
    );
    git(dir.path(), &["status"]);
    git(dir.path(), &["fsck", "--full"]);

    let dry = git2(dir.path(), &["undo", "--dry-run"], None);
    assert!(dry.status.success());
    let dry_text = String::from_utf8(dry.stdout).unwrap();
    assert!(dry_text.contains("replace worktree file.txt"));
    assert_eq!(
        fs::read(dir.path().join("file.txt")).unwrap(),
        b"commit 1\n"
    );

    let undo = git2(dir.path(), &["undo"], None);
    assert!(undo.status.success());
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        raw_index_before
    );
    assert_eq!(
        fs::read(dir.path().join("file.txt")).unwrap(),
        b"dirty worktree\n"
    );
    assert_eq!(
        fs::read(dir.path().join("swap/inner.txt")).unwrap(),
        b"swap dir\n"
    );

    // Redo by undoing the undo
    let redo = git2(dir.path(), &["undo"], None);
    assert!(redo.status.success());
    assert_eq!(
        fs::read(dir.path().join("file.txt")).unwrap(),
        b"commit 1\n"
    );
    assert_eq!(
        fs::read(dir.path().join("note.ignored")).unwrap(),
        b"keep ignored\n"
    );

    let oplog = git2(dir.path(), &["oplog"], None);
    assert!(oplog.status.success());
    let text = String::from_utf8(oplog.stdout).unwrap();
    assert!(text.contains("git2:reset --hard HEAD~1"));
    assert!(text.contains("git2:undo"));
}

#[test]
fn cli_reset_and_undo_crash_recovery_matrix() {
    for failpoint in [
        "abort:after_prepare",
        "abort:before_action:1",
        "abort:after_head_selection",
    ] {
        let (dir, c1, _) = init_two_commit_repo();
        let crashed = git2(dir.path(), &["reset", "--hard", "HEAD~1"], Some(failpoint));
        assert!(!crashed.status.success(), "{failpoint} must abort");
        assert!(dir.path().join(".git/git2/pending/active").is_file());

        let rec = git2(dir.path(), &["recover"], None);
        assert!(
            rec.status.success(),
            "recover after {failpoint} failed: {}",
            String::from_utf8_lossy(&rec.stderr)
        );
        assert!(!dir.path().join(".git/git2/pending/active").exists());
        if failpoint == "abort:after_prepare" {
            assert_eq!(
                fs::read(dir.path().join("file.txt")).unwrap(),
                b"dirty worktree\n"
            );
        } else {
            assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]).trim(), c1);
            assert_eq!(
                fs::read(dir.path().join("file.txt")).unwrap(),
                b"commit 1\n"
            );
            assert_eq!(fs::read(dir.path().join("swap")).unwrap(), b"swap file\n");
        }
        git(dir.path(), &["fsck", "--full"]);
    }

    // I/O error during reset finalization rolls back cleanly via `recover --rollback`
    let (dir_io, _, c2) = init_two_commit_repo();
    let io_fail = git2(
        dir_io.path(),
        &["reset", "--hard", "HEAD~1"],
        Some("io:before_finalize"),
    );
    assert!(!io_fail.status.success());
    let rb = git2(dir_io.path(), &["recover", "--rollback"], None);
    assert!(
        rb.status.success(),
        "{}",
        String::from_utf8_lossy(&rb.stderr)
    );
    assert_eq!(git(dir_io.path(), &["rev-parse", "HEAD"]).trim(), c2);
    assert_eq!(
        fs::read(dir_io.path().join("file.txt")).unwrap(),
        b"dirty worktree\n"
    );
    git(dir_io.path(), &["fsck", "--full"]);

    // Crash mid-`git2 undo` recovers cleanly via `git2 recover`
    let (dir_undo, _, c2_undo) = init_two_commit_repo();
    assert!(
        git2(dir_undo.path(), &["reset", "--hard", "HEAD~1"], None)
            .status
            .success()
    );
    let crashed_undo = git2(dir_undo.path(), &["undo"], Some("abort:before_action:1"));
    assert!(!crashed_undo.status.success());
    let rec_undo = git2(dir_undo.path(), &["recover"], None);
    assert!(
        rec_undo.status.success(),
        "{}",
        String::from_utf8_lossy(&rec_undo.stderr)
    );
    assert_eq!(git(dir_undo.path(), &["rev-parse", "HEAD"]).trim(), c2_undo);
    assert_eq!(
        fs::read(dir_undo.path().join("file.txt")).unwrap(),
        b"dirty worktree\n"
    );
    assert_eq!(
        fs::read(dir_undo.path().join("swap/inner.txt")).unwrap(),
        b"swap dir\n"
    );
    git(dir_undo.path(), &["fsck", "--full"]);
}
