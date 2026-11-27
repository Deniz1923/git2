#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git2_isolated(directory: &Path, arguments: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git2"));
    cmd.args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for &(k, v) in envs {
        cmd.env(k, v);
    }
    cmd.output().expect("run git2")
}

fn git2(directory: &Path, arguments: &[&str]) -> Output {
    git2_isolated(
        directory,
        arguments,
        &[
            ("GIT_AUTHOR_NAME", "CLI Test"),
            ("GIT_AUTHOR_EMAIL", "cli@example.invalid"),
            ("GIT_COMMITTER_NAME", "CLI Test"),
            ("GIT_COMMITTER_EMAIL", "cli@example.invalid"),
        ],
    )
}

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "CLI Test")
        .env("GIT_AUTHOR_EMAIL", "cli@example.invalid")
        .env("GIT_COMMITTER_NAME", "CLI Test")
        .env("GIT_COMMITTER_EMAIL", "cli@example.invalid")
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
fn cli_commit_log_undo_and_identity_precedence() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet"]);
    fs::write(dir.path().join("hello.txt"), b"hello\n").unwrap();

    assert!(git2(dir.path(), &["adopt"]).status.success());
    assert!(git2(dir.path(), &["add", "hello.txt"]).status.success());

    // Without config or env identity, commit must fail clearly without modifying HEAD.
    let missing_id = git2_isolated(
        dir.path(),
        &["commit", "-m", "missing identity"],
        &[
            ("GIT_AUTHOR_NAME", ""),
            ("GIT_AUTHOR_EMAIL", ""),
            ("GIT_COMMITTER_NAME", ""),
            ("GIT_COMMITTER_EMAIL", ""),
        ],
    );
    assert!(!missing_id.status.success());
    assert!(
        String::from_utf8_lossy(&missing_id.stderr).contains("identity")
            || String::from_utf8_lossy(&missing_id.stderr).contains("author name")
    );

    // Env overrides take precedence over config and handle raw/ISO dates cleanly.
    let committed = git2_isolated(
        dir.path(),
        &["commit", "-m", "cli root commit"],
        &[
            ("GIT_AUTHOR_NAME", "Author Person"),
            ("GIT_AUTHOR_EMAIL", "author@example.invalid"),
            ("GIT_AUTHOR_DATE", "1700000000 +0330"),
            ("GIT_COMMITTER_NAME", "Committer Person"),
            ("GIT_COMMITTER_EMAIL", "committer@example.invalid"),
            ("GIT_COMMITTER_DATE", "2024-05-01T12:00:00Z"),
        ],
    );
    assert!(
        committed.status.success(),
        "commit failed:\n{}",
        String::from_utf8_lossy(&committed.stderr)
    );

    let cat = String::from_utf8(git(dir.path(), &["cat-file", "-p", "HEAD"]).stdout).unwrap();
    assert!(cat.contains("author Author Person <author@example.invalid> 1700000000 +0330\n"));
    assert!(
        cat.contains("committer Committer Person <committer@example.invalid> 1714564800 +0000\n")
    );

    let logged = git2(dir.path(), &["log", "-n", "1"]);
    assert!(logged.status.success());
    let log_text = String::from_utf8(logged.stdout).unwrap();
    assert!(log_text.contains("Author: Author Person <author@example.invalid>"));
    assert!(log_text.contains("cli root commit"));

    let undone = git2(dir.path(), &["undo"]);
    assert!(undone.status.success());
    assert!(!git2(dir.path(), &["log"]).status.success());

    let redone = git2(dir.path(), &["undo"]);
    assert!(redone.status.success());
    assert!(git2(dir.path(), &["log"]).status.success());

    let oplog = String::from_utf8(git2(dir.path(), &["oplog"]).stdout).unwrap();
    assert!(oplog.contains("git2:commit"));
    assert!(oplog.contains("git2:undo"));
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn cli_commit_abort_failpoints_recover_and_rollback_cleanly() {
    for failpoint in [
        "abort:after_prepare",
        "abort:before_action:0",
        "abort:after_action:0",
        "abort:before_finalize",
        "abort:after_operation_write",
        "abort:after_head_selection",
        "abort:before_pending_cleanup",
    ] {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        fs::write(dir.path().join("file.txt"), b"v1\n").unwrap();
        assert!(git2(dir.path(), &["adopt"]).status.success());
        assert!(git2(dir.path(), &["add", "--all"]).status.success());
        assert!(git2(dir.path(), &["commit", "-m", "base"]).status.success());

        fs::write(dir.path().join("file.txt"), b"v2\n").unwrap();
        assert!(git2(dir.path(), &["add", "--all"]).status.success());

        let killed = git2_isolated(
            dir.path(),
            &["commit", "-m", "crash test"],
            &[
                ("GIT_AUTHOR_NAME", "Crash Test"),
                ("GIT_AUTHOR_EMAIL", "crash@example.invalid"),
                ("GIT_COMMITTER_NAME", "Crash Test"),
                ("GIT_COMMITTER_EMAIL", "crash@example.invalid"),
                ("GIT2_TEST_FAILPOINT", failpoint),
            ],
        );
        assert!(
            !killed.status.success(),
            "{failpoint} should abort commit process"
        );

        // Mutating commands must block until recover runs.
        assert!(!git2(dir.path(), &["log"]).status.success());
        let status = git2(dir.path(), &["recover", "--status"]);
        assert!(status.status.success());
        assert!(String::from_utf8_lossy(&status.stdout).contains("command: commit"));

        if failpoint == "abort:after_prepare" {
            let rb = git2(dir.path(), &["recover", "--rollback"]);
            assert!(
                rb.status.success(),
                "rollback failed for {failpoint}:\n{}",
                String::from_utf8_lossy(&rb.stderr)
            );
            let logs = String::from_utf8(git2(dir.path(), &["log"]).stdout).unwrap();
            assert!(!logs.contains("crash test"));
            assert!(logs.contains("base"));
        } else {
            let rec = git2(dir.path(), &["recover"]);
            assert!(
                rec.status.success(),
                "recover failed for {failpoint}:\n{}",
                String::from_utf8_lossy(&rec.stderr)
            );
            let logs = String::from_utf8(git2(dir.path(), &["log"]).stdout).unwrap();
            assert!(logs.contains("crash test"));
        }
        git(dir.path(), &["fsck", "--full", "--strict"]);
    }
}
