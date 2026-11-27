#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git2(directory: &Path, arguments: &[&str], extra_env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git2"));
    cmd.args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "CLI Net Test")
        .env("GIT_AUTHOR_EMAIL", "netcli@example.invalid")
        .env("GIT_COMMITTER_NAME", "CLI Net Test")
        .env("GIT_COMMITTER_EMAIL", "netcli@example.invalid");
    for &(k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("run git2")
}

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "CLI Net Test")
        .env("GIT_AUTHOR_EMAIL", "netcli@example.invalid")
        .env("GIT_COMMITTER_NAME", "CLI Net Test")
        .env("GIT_COMMITTER_EMAIL", "netcli@example.invalid")
        .output()
        .expect("Git must be installed for CLI network tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn setup_remote(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let bare = root.join("remote.git");
    let seed = root.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(
        root,
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            bare.to_str().unwrap(),
        ],
    );
    git(&seed, &["init", "--quiet", "-b", "main"]);
    fs::write(seed.join("app.txt"), b"v1\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "v1"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);
    (bare, seed)
}

#[test]
fn cli_clone_remote_fetch_pull_conflict_rollback_push_and_oplog_flow() {
    let root = tempfile::tempdir().unwrap();
    let (bare, seed) = setup_remote(root.path());

    let cloned = git2(root.path(), &["clone", bare.to_str().unwrap(), "work"], &[]);
    assert!(
        cloned.status.success(),
        "git2 clone failed:\n{}",
        String::from_utf8_lossy(&cloned.stderr)
    );
    let work = root.path().join("work");
    assert!(
        String::from_utf8(cloned.stdout)
            .unwrap()
            .contains("adopted 1 files")
    );
    git(&work, &["config", "user.name", "CLI Net Test"]);
    git(&work, &["config", "user.email", "netcli@example.invalid"]);

    let rem_list = git2(&work, &["remote", "-v"], &[]);
    assert!(rem_list.status.success());
    assert!(
        String::from_utf8(rem_list.stdout)
            .unwrap()
            .contains("origin")
    );

    // Push a commit upstream, fetch & pull --ff-only via CLI.
    fs::write(seed.join("app.txt"), b"v2\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "v2"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);

    assert!(git2(&work, &["fetch", "origin"], &[]).status.success());
    let pulled = git2(&work, &["pull", "origin", "main"], &[]);
    assert!(pulled.status.success());
    assert_eq!(fs::read(work.join("app.txt")).unwrap(), b"v2\n");

    // Local commit & push emits notice and refuses undo.
    fs::write(work.join("app.txt"), b"v3\n").unwrap();
    assert!(git2(&work, &["add", "app.txt"], &[]).status.success());
    assert!(git2(&work, &["commit", "-m", "v3"], &[]).status.success());

    let pushed = git2(&work, &["push", "-u", "origin", "main"], &[]);
    assert!(pushed.status.success());
    assert!(
        String::from_utf8_lossy(&pushed.stderr).contains("cannot be undone by local `git2 undo`")
    );
    assert!(!git2(&work, &["undo"], &[]).status.success());

    // Conflict during `git2 pull --merge` -> `git2 recover --status` -> `git2 recover --rollback`.
    git(
        &seed,
        &[
            "pull",
            "--quiet",
            "--ff-only",
            bare.to_str().unwrap(),
            "main",
        ],
    );
    fs::write(seed.join("app.txt"), b"seed conflict\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "seed conflict"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);

    fs::write(work.join("app.txt"), b"work conflict\n").unwrap();
    assert!(git2(&work, &["add", "app.txt"], &[]).status.success());
    assert!(
        git2(&work, &["commit", "-m", "work conflict"], &[])
            .status
            .success()
    );

    let conflict_pull = git2(&work, &["pull", "--merge", "origin", "main"], &[]);
    assert!(!conflict_pull.status.success());

    let status = git2(&work, &["recover", "--status"], &[]);
    assert!(status.status.success());
    let status_text = String::from_utf8(status.stdout).unwrap();
    assert!(status_text.contains("phase: delegated"));
    assert!(status_text.contains("conflict state: present"));

    let rb = git2(&work, &["recover", "--rollback"], &[]);
    assert!(
        rb.status.success(),
        "rollback failed:\n{}",
        String::from_utf8_lossy(&rb.stderr)
    );
    assert_eq!(fs::read(work.join("app.txt")).unwrap(), b"work conflict\n");
    assert!(!work.join(".git/MERGE_HEAD").exists());
    git(&work, &["fsck", "--full", "--strict"]);
}

#[test]
fn cli_redacts_url_credentials_and_recovers_delegated_process_kills() {
    let root = tempfile::tempdir().unwrap();
    let (bare, seed) = setup_remote(root.path());

    // 1. Credential redaction in pending/delegated & recover --status.
    assert!(
        git2(
            root.path(),
            &["clone", bare.to_str().unwrap(), "creds"],
            &[]
        )
        .status
        .success()
    );
    let creds_work = root.path().join("creds");
    let killed_cred = git2(
        &creds_work,
        &[
            "fetch",
            "https://alice:TOPSECRET@example.invalid/org/repo.git",
        ],
        &[("GIT2_TEST_FAILPOINT", "abort:after_prepare")],
    );
    assert!(!killed_cred.status.success());
    let status = git2(&creds_work, &["recover", "--status"], &[]);
    assert!(status.status.success());
    let status_out = String::from_utf8(status.stdout).unwrap();
    assert!(!status_out.contains("TOPSECRET"));
    assert!(status_out.contains("https://***@example.invalid/org/repo.git"));
    assert!(git2(&creds_work, &["recover"], &[]).status.success());

    // 2. Crash recovery matrix across delegated `fetch` boundaries.
    for failpoint in [
        "abort:after_prepare",
        "abort:before_finalize",
        "abort:after_head_selection",
        "abort:before_pending_cleanup",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (remote, seed_repo) = setup_remote(dir.path());
        assert!(
            git2(
                dir.path(),
                &["clone", remote.to_str().unwrap(), "repo"],
                &[]
            )
            .status
            .success()
        );
        let work = dir.path().join("repo");

        fs::write(seed_repo.join("app.txt"), b"upstream update\n").unwrap();
        git(&seed_repo, &["add", "app.txt"]);
        git(&seed_repo, &["commit", "--quiet", "-m", "upstream update"]);
        git(
            &seed_repo,
            &["push", "--quiet", remote.to_str().unwrap(), "main"],
        );

        let killed = git2(
            &work,
            &["fetch", "origin"],
            &[("GIT2_TEST_FAILPOINT", failpoint)],
        );
        assert!(!killed.status.success(), "{failpoint} must abort child");

        let pending = git2(&work, &["recover", "--status"], &[]);
        assert!(pending.status.success());
        assert!(
            String::from_utf8(pending.stdout)
                .unwrap()
                .contains("phase: delegated")
        );

        if failpoint == "abort:after_prepare" {
            let rb = git2(&work, &["recover", "--rollback"], &[]);
            assert!(rb.status.success());
        } else {
            let rec = git2(&work, &["recover"], &[]);
            assert!(
                rec.status.success(),
                "recover failed at {failpoint}:\n{}",
                String::from_utf8_lossy(&rec.stderr)
            );
            let oplog = String::from_utf8(git2(&work, &["oplog"], &[]).stdout).unwrap();
            assert_eq!(oplog.matches("git2:fetch origin").count(), 1);
        }
        git(&work, &["fsck", "--full", "--strict"]);
    }
    let _ = seed;
}
