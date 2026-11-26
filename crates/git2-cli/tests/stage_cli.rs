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

fn init_stage_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    fs::write(dir.path().join("base.txt"), b"base\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "initial"]);
    assert!(git2(dir.path(), &["adopt"], None).status.success());

    fs::write(dir.path().join("-dash.txt"), b"dash file\n").unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/file.txt"), b"sub file\n").unwrap();
    dir
}

#[test]
fn cli_add_dash_subdir_all_and_undo() {
    let dir = init_stage_repo();

    let add_dash = git2(dir.path(), &["add", "--", "-dash.txt"], None);
    assert!(
        add_dash.status.success(),
        "{}",
        String::from_utf8_lossy(&add_dash.stderr)
    );

    let add_sub = Command::new(env!("CARGO_BIN_EXE_git2"))
        .args(["add", "file.txt"])
        .current_dir(dir.path().join("sub"))
        .output()
        .unwrap();
    assert!(
        add_sub.status.success(),
        "{}",
        String::from_utf8_lossy(&add_sub.stderr)
    );

    let cached = git(dir.path(), &["diff", "--cached", "--name-only"]);
    assert!(cached.contains("-dash.txt"));
    assert!(cached.contains("sub/file.txt"));

    // Delete `base.txt` and stage via `--all`
    fs::remove_file(dir.path().join("base.txt")).unwrap();
    let add_all = git2(dir.path(), &["add", "--all"], None);
    assert!(add_all.status.success());
    let status = git(dir.path(), &["status", "--short"]);
    assert!(status.contains("D  base.txt"));

    // Undo restores index prior to `add --all`
    let undo_out = git2(dir.path(), &["undo"], None);
    assert!(undo_out.status.success());
    let status_after_undo = git(dir.path(), &["status", "--short"]);
    assert!(status_after_undo.contains(" D base.txt"));

    let oplog = git2(dir.path(), &["oplog"], None);
    let log_text = String::from_utf8(oplog.stdout).unwrap();
    assert!(log_text.contains("git2:add -dash.txt"));
    assert!(log_text.contains("git2:add --all"));
    assert!(log_text.contains("git2:undo"));
    git(dir.path(), &["fsck", "--full"]);
}

#[test]
fn cli_add_crash_recovery_and_io_rollback() {
    for failpoint in [
        "abort:after_prepare",
        "abort:after_lock_publish:2",
        "abort:after_head_selection",
    ] {
        let dir = init_stage_repo();
        let crashed = git2(dir.path(), &["add", "--all"], Some(failpoint));
        assert!(!crashed.status.success(), "{failpoint} must abort");
        assert!(dir.path().join(".git/git2/pending/active").is_file());

        let rec = git2(dir.path(), &["recover"], None);
        assert!(
            rec.status.success(),
            "recover after {failpoint} failed: {}",
            String::from_utf8_lossy(&rec.stderr)
        );
        assert!(!dir.path().join(".git/git2/pending/active").exists());
        let cached = git(dir.path(), &["diff", "--cached", "--name-only"]);
        if failpoint == "abort:after_prepare" {
            assert!(cached.is_empty());
        } else {
            assert!(cached.contains("-dash.txt"));
            assert!(cached.contains("sub/file.txt"));
        }
        git(dir.path(), &["fsck", "--full"]);
    }

    // I/O error before finalize rolls back index cleanly
    let dir_io = init_stage_repo();
    let io_fail = git2(dir_io.path(), &["add", "--all"], Some("io:before_finalize"));
    assert!(!io_fail.status.success());
    let rb = git2(dir_io.path(), &["recover", "--rollback"], None);
    assert!(
        rb.status.success(),
        "{}",
        String::from_utf8_lossy(&rb.stderr)
    );
    assert!(
        git(dir_io.path(), &["diff", "--cached", "--name-only"]).is_empty(),
        "rollback must revert staged entries"
    );
    git(dir_io.path(), &["fsck", "--full"]);
}
