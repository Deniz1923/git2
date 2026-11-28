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

fn setup_two_branch_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "CLI Test"]);
    git(dir.path(), &["config", "user.email", "cli@example.invalid"]);
    fs::write(dir.path().join("f.txt"), b"v1\n").unwrap();
    git(dir.path(), &["add", "f.txt"]);
    git(dir.path(), &["commit", "--quiet", "-m", "v1 commit"]);

    assert!(git2(dir.path(), &["adopt"]).status.success());
    assert!(
        git2(dir.path(), &["switch", "-c", "feature"])
            .status
            .success()
    );
    fs::write(dir.path().join("f.txt"), b"v2\n").unwrap();
    fs::write(dir.path().join("extra.txt"), b"extra\n").unwrap();
    assert!(git2(dir.path(), &["add", "--all"]).status.success());
    assert!(
        git2(dir.path(), &["commit", "-m", "v2 commit"])
            .status
            .success()
    );
    dir
}

#[test]
fn cli_branch_switch_tag_workflow_and_undo() {
    let dir = setup_two_branch_repo();

    let branches = git2(dir.path(), &["branch", "-v"]);
    assert!(branches.status.success());
    let branch_text = String::from_utf8(branches.stdout).unwrap();
    assert!(branch_text.contains("* feature "));
    assert!(branch_text.contains("  main "));

    // Annotated and lightweight tags via CLI.
    assert!(
        git2(dir.path(), &["tag", "-a", "rel-1", "-m", "release one"])
            .status
            .success()
    );
    assert!(
        git2(dir.path(), &["tag", "light-1", "main"])
            .status
            .success()
    );
    let tags_out = String::from_utf8(git2(dir.path(), &["tag", "-l"]).stdout).unwrap();
    assert_eq!(tags_out, "light-1\nrel-1\n");

    // Rename, switch back to main, verify already-on message, and undo.
    assert!(
        git2(dir.path(), &["branch", "-m", "renamed-feat"])
            .status
            .success()
    );
    let switched = git2(dir.path(), &["switch", "main"]);
    assert!(switched.status.success());
    assert!(
        String::from_utf8(switched.stdout)
            .unwrap()
            .contains("Switched to branch 'main'")
    );
    assert!(!dir.path().join("extra.txt").exists());
    assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"v1\n");

    let already = git2(dir.path(), &["switch", "main"]);
    assert!(already.status.success());
    assert!(
        String::from_utf8(already.stdout)
            .unwrap()
            .contains("Already on 'main'")
    );

    assert!(git2(dir.path(), &["undo"]).status.success());
    assert!(dir.path().join("extra.txt").exists());
    assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"v2\n");

    // Force-delete unmerged branch from main, delete tag, and verify oplog.
    assert!(git2(dir.path(), &["switch", "main"]).status.success());
    assert!(
        !git2(dir.path(), &["branch", "-d", "renamed-feat"])
            .status
            .success()
    );
    assert!(
        git2(dir.path(), &["branch", "-D", "renamed-feat"])
            .status
            .success()
    );
    assert!(git2(dir.path(), &["tag", "-d", "rel-1"]).status.success());

    let oplog = String::from_utf8(git2(dir.path(), &["oplog"]).stdout).unwrap();
    assert!(oplog.contains("git2:switch"));
    assert!(oplog.contains("git2:branch -D renamed-feat"));
    assert!(oplog.contains("git2:tag -d rel-1"));
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn cli_switch_tag_and_branch_rename_crash_failpoints_recover_cleanly() {
    // 1. Switch across tree diff interrupted at multiple boundaries:
    // `after_prepare` aborts cleanly back to `feature`; `Applying+` completes forward to `main`.
    for (failpoint, expected_branch, expected_file, expected_extra) in [
        ("abort:after_prepare", "feature", b"v2\n".as_slice(), true),
        ("abort:before_action:1", "main", b"v1\n".as_slice(), false),
        ("abort:after_action:1", "main", b"v1\n".as_slice(), false),
        ("abort:before_finalize", "main", b"v1\n".as_slice(), false),
        (
            "abort:after_head_selection",
            "main",
            b"v1\n".as_slice(),
            false,
        ),
    ] {
        let dir = setup_two_branch_repo();
        let killed = git2_isolated(
            dir.path(),
            &["switch", "main"],
            &[("GIT2_TEST_FAILPOINT", failpoint)],
        );
        assert!(!killed.status.success(), "{failpoint} should abort switch");
        assert!(git2(dir.path(), &["recover"]).status.success());
        assert_eq!(
            String::from_utf8(git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
                .unwrap()
                .trim(),
            expected_branch
        );
        assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), expected_file);
        assert_eq!(dir.path().join("extra.txt").exists(), expected_extra);
        git(dir.path(), &["fsck", "--full", "--strict"]);
    }

    // 2. Rollback an interrupted switch back to `feature`.
    let dir_rb = setup_two_branch_repo();
    let killed_rb = git2_isolated(
        dir_rb.path(),
        &["switch", "main"],
        &[("GIT2_TEST_FAILPOINT", "abort:after_action:1")],
    );
    assert!(!killed_rb.status.success());
    assert!(
        git2(dir_rb.path(), &["recover", "--rollback"])
            .status
            .success()
    );
    assert_eq!(
        String::from_utf8(git(dir_rb.path(), &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
            .unwrap()
            .trim(),
        "feature"
    );
    assert_eq!(fs::read(dir_rb.path().join("f.txt")).unwrap(), b"v2\n");
    assert!(dir_rb.path().join("extra.txt").exists());
    git(dir_rb.path(), &["fsck", "--full", "--strict"]);

    // 3. Annotated tag object rehydration + ref creation interrupted mid-flight -> recover + undo.
    let dir_tag = setup_two_branch_repo();
    let killed_tag = git2_isolated(
        dir_tag.path(),
        &["tag", "-a", "v1.0", "-m", "crash tag"],
        &[
            ("GIT_AUTHOR_NAME", "CLI Test"),
            ("GIT_AUTHOR_EMAIL", "cli@example.invalid"),
            ("GIT_COMMITTER_NAME", "CLI Test"),
            ("GIT_COMMITTER_EMAIL", "cli@example.invalid"),
            ("GIT2_TEST_FAILPOINT", "abort:after_action:0"),
        ],
    );
    assert!(!killed_tag.status.success());
    assert!(git2(dir_tag.path(), &["recover"]).status.success());
    assert_eq!(
        String::from_utf8(git2(dir_tag.path(), &["tag", "-l"]).stdout).unwrap(),
        "v1.0\n"
    );
    assert!(git2(dir_tag.path(), &["undo"]).status.success());
    assert_eq!(
        String::from_utf8(git2(dir_tag.path(), &["tag", "-l"]).stdout).unwrap(),
        ""
    );
    git(dir_tag.path(), &["fsck", "--full", "--strict"]);

    // 4. Branch D/F rename (`main` -> `team/main`) interrupted between RemoveRef and WriteRef -> rollback!
    let dir_ren = setup_two_branch_repo();
    let killed_ren = git2_isolated(
        dir_ren.path(),
        &["branch", "-m", "main", "team/main"],
        &[("GIT2_TEST_FAILPOINT", "abort:after_action:0")],
    );
    assert!(!killed_ren.status.success());
    assert!(
        git2(dir_ren.path(), &["recover", "--rollback"])
            .status
            .success()
    );
    let branches = String::from_utf8(git2(dir_ren.path(), &["branch"]).stdout).unwrap();
    assert!(branches.contains("  main\n"));
    assert!(!branches.contains("team/main"));
    git(dir_ren.path(), &["fsck", "--full", "--strict"]);
}
