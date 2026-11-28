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
        .env("GIT_AUTHOR_NAME", "Release Check")
        .env("GIT_AUTHOR_EMAIL", "release@example.invalid")
        .env("GIT_COMMITTER_NAME", "Release Check")
        .env("GIT_COMMITTER_EMAIL", "release@example.invalid");
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
        .env("GIT_AUTHOR_NAME", "Release Check")
        .env("GIT_AUTHOR_EMAIL", "release@example.invalid")
        .env("GIT_COMMITTER_NAME", "Release Check")
        .env("GIT_COMMITTER_EMAIL", "release@example.invalid")
        .output()
        .expect("Git must be installed for release readiness tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn mirror_push_never_leaks_sidecar_refs_or_staged_only_objects() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    let bare = root.path().join("remote.git");
    fs::create_dir_all(&work).unwrap();

    git(&work, &["init", "--quiet", "-b", "main"]);
    git(
        root.path(),
        &["init", "--quiet", "--bare", bare.to_str().unwrap()],
    );
    fs::write(work.join("public.txt"), b"public v1\n").unwrap();
    git(&work, &["add", "public.txt"]);
    git(&work, &["commit", "--quiet", "-m", "public v1"]);

    assert!(git2(&work, &["adopt"], &[]).status.success());

    fs::write(work.join("public.txt"), b"public v2\n").unwrap();
    assert!(git2(&work, &["add", "public.txt"], &[]).status.success());
    assert!(
        git2(&work, &["commit", "-m", "public v2"], &[])
            .status
            .success()
    );
    assert!(git2(&work, &["checkpoint"], &[]).status.success());

    // Stage an uncommitted secret blob that should remain private in local recovery history,
    // then discard it via reset --hard HEAD~1 and undo/redo around it.
    let secret_payload = b"LOCAL_RECOVERY_ONLY_SECRET_DO_NOT_PUSH\n";
    fs::write(work.join("secret.txt"), secret_payload).unwrap();
    let secret_oid = String::from_utf8(git(&work, &["hash-object", "secret.txt"]).stdout).unwrap();
    let secret_oid = secret_oid.trim();

    assert!(git2(&work, &["add", "secret.txt"], &[]).status.success());
    assert!(
        git2(&work, &["reset", "--hard", "HEAD~1"], &[])
            .status
            .success()
    );
    assert!(git2(&work, &["undo"], &[]).status.success());
    assert!(
        git2(&work, &["reset", "--hard", "HEAD"], &[])
            .status
            .success()
    );

    let remote_url = bare.to_str().unwrap();
    git(&work, &["push", "--quiet", "--mirror", remote_url]);

    assert!(
        !bare.join("git2").exists(),
        "mirror push must never copy .git/git2/ into remote"
    );
    let refs = String::from_utf8(git(&bare, &["for-each-ref"]).stdout).unwrap();
    assert!(
        !refs.contains("refs/git2"),
        "remote refs must not contain git2 namespace:\n{refs}"
    );
    let check_secret = Command::new("git")
        .args(["cat-file", "-e", secret_oid])
        .current_dir(&bare)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        !check_secret.status.success(),
        "staged-only/recovery-only secret blob {secret_oid} leaked to bare remote on push --mirror"
    );
    git(&bare, &["fsck", "--full", "--strict"]);
}

#[test]
fn empty_path_smoke_test_proves_zero_runtime_dependency_on_git() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Offline User"]);
    git(
        dir.path(),
        &["config", "user.email", "offline@example.invalid"],
    );
    fs::write(dir.path().join("v1.txt"), b"v1\n").unwrap();
    git(dir.path(), &["add", "v1.txt"]);
    git(dir.path(), &["commit", "--quiet", "-m", "v1"]);

    let no_path = [("PATH", "")];

    let adopt = git2(dir.path(), &["adopt"], &no_path);
    assert!(
        adopt.status.success(),
        "adopt: {}",
        String::from_utf8_lossy(&adopt.stderr)
    );
    let adopt_stdout = String::from_utf8(adopt.stdout).unwrap();
    let (_, baseline_view) = adopt_stdout.trim_end().rsplit_once(" as view ").unwrap();

    fs::write(dir.path().join("v1.txt"), b"v1 edited\n").unwrap();
    fs::write(dir.path().join("v2.txt"), b"v2 new\n").unwrap();

    let status = git2(dir.path(), &["status", "-z"], &no_path);
    assert!(
        status.status.success(),
        "status: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(status.stdout, b" M v1.txt\0?? v2.txt\0");

    assert!(
        git2(dir.path(), &["add", "--all"], &no_path)
            .status
            .success()
    );
    assert!(
        git2(dir.path(), &["commit", "-m", "offline commit"], &no_path)
            .status
            .success()
    );

    let log = git2(dir.path(), &["log", "-n", "5"], &no_path);
    assert!(
        log.status.success(),
        "log: {}",
        String::from_utf8_lossy(&log.stderr)
    );
    let log_str = String::from_utf8(log.stdout).unwrap();
    assert!(log_str.contains("offline commit"));
    assert!(log_str.contains("v1"));

    assert!(git2(dir.path(), &["checkpoint"], &no_path).status.success());
    assert!(
        git2(dir.path(), &["reset", "--hard", "HEAD~1"], &no_path)
            .status
            .success()
    );
    assert_eq!(fs::read(dir.path().join("v1.txt")).unwrap(), b"v1\n");
    assert!(!dir.path().join("v2.txt").exists());

    assert!(git2(dir.path(), &["undo"], &no_path).status.success());
    assert_eq!(fs::read(dir.path().join("v1.txt")).unwrap(), b"v1 edited\n");
    assert_eq!(fs::read(dir.path().join("v2.txt")).unwrap(), b"v2 new\n");

    verify_offline_branch_switch_tag(dir.path(), &no_path);

    let dry_restore = git2(
        dir.path(),
        &["restore", "--view", baseline_view, "--dry-run"],
        &no_path,
    );
    assert!(
        dry_restore.status.success(),
        "restore --dry-run: {}",
        String::from_utf8_lossy(&dry_restore.stderr)
    );
    assert!(!String::from_utf8(dry_restore.stdout).unwrap().is_empty());

    let oplog = git2(dir.path(), &["oplog"], &no_path);
    assert!(oplog.status.success());
    assert!(
        String::from_utf8(oplog.stdout)
            .unwrap()
            .contains("git2:commit")
    );

    let rec_status = git2(dir.path(), &["recover", "--status"], &no_path);
    assert!(rec_status.status.success());
    assert_eq!(
        String::from_utf8(rec_status.stdout).unwrap().trim(),
        "no pending transaction"
    );

    git(dir.path(), &["fsck", "--full", "--strict"]);
}

fn verify_offline_branch_switch_tag(repo_dir: &Path, no_path: &[(&str, &str)]) {
    assert!(
        git2(repo_dir, &["branch", "topic"], no_path)
            .status
            .success()
    );
    assert!(
        git2(repo_dir, &["switch", "topic"], no_path)
            .status
            .success()
    );
    assert!(
        git2(
            repo_dir,
            &["tag", "-a", "v0.2-test", "-m", "offline tag"],
            no_path
        )
        .status
        .success()
    );
    assert!(
        git2(repo_dir, &["tag", "v0.2-light"], no_path)
            .status
            .success()
    );
    let tags = git2(repo_dir, &["tag"], no_path);
    assert!(tags.status.success());
    assert_eq!(
        String::from_utf8(tags.stdout).unwrap(),
        "v0.2-light\nv0.2-test\n"
    );
    assert!(
        git2(repo_dir, &["switch", "main"], no_path)
            .status
            .success()
    );
    assert!(
        git2(repo_dir, &["branch", "-d", "topic"], no_path)
            .status
            .success()
    );
    assert!(git2(repo_dir, &["undo"], no_path).status.success());
}
