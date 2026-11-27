#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

use git2_core::repository::LocalRepository;

fn git(directory: &Path, arguments: &[&str]) -> Output {
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
        .expect("Git must be installed for status tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn two_column_status_matches_git_for_core_change_types() {
    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    std::fs::write(directory.path().join("both.txt"), b"base\n").unwrap();
    std::fs::write(directory.path().join("deleted.txt"), b"base\n").unwrap();
    git(directory.path(), &["add", "both.txt", "deleted.txt"]);
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

    std::fs::write(directory.path().join("both.txt"), b"staged\n").unwrap();
    git(directory.path(), &["add", "both.txt"]);
    std::fs::write(directory.path().join("both.txt"), b"unstaged too\n").unwrap();
    std::fs::remove_file(directory.path().join("deleted.txt")).unwrap();
    std::fs::write(directory.path().join("new-staged.txt"), b"new\n").unwrap();
    git(directory.path(), &["add", "new-staged.txt"]);
    std::fs::write(directory.path().join("untracked.txt"), b"new\n").unwrap();

    let expected = git(
        directory.path(),
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    let actual = LocalRepository::discover(directory.path())
        .unwrap()
        .status()
        .unwrap()
        .porcelain_v1(false);
    assert_eq!(actual, expected.stdout);
}

#[test]
fn two_column_status_reports_staged_type_changes_as_t() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet"]);
    std::fs::write(directory.path().join("to_link"), b"file\n").unwrap();
    symlink("target", directory.path().join("to_file")).unwrap();
    std::fs::write(directory.path().join("chmod_only"), b"script\n").unwrap();
    git(
        directory.path(),
        &["add", "to_link", "to_file", "chmod_only"],
    );
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

    std::fs::remove_file(directory.path().join("to_link")).unwrap();
    symlink("new_target", directory.path().join("to_link")).unwrap();
    std::fs::remove_file(directory.path().join("to_file")).unwrap();
    std::fs::write(directory.path().join("to_file"), b"now regular\n").unwrap();
    let mut perms = std::fs::metadata(directory.path().join("chmod_only"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(directory.path().join("chmod_only"), perms).unwrap();
    git(
        directory.path(),
        &["add", "to_link", "to_file", "chmod_only"],
    );

    let expected = git(
        directory.path(),
        &["status", "--porcelain=v1", "--untracked-files=all"],
    );
    let actual = LocalRepository::discover(directory.path())
        .unwrap()
        .status()
        .unwrap()
        .porcelain_v1(false);
    assert_eq!(actual, expected.stdout);
    let text = String::from_utf8(actual).unwrap();
    assert!(text.contains("T  to_link\n"), "{text}");
    assert!(text.contains("T  to_file\n"), "{text}");
    assert!(text.contains("M  chmod_only\n"), "{text}");
}

#[test]
fn status_matches_git_on_unborn_head_with_nested_and_global_ignores() {
    let directory = tempfile::tempdir().unwrap();
    let excludes = directory.path().join(".global_excludes");
    std::fs::write(&excludes, b"*.secret\n").unwrap();

    git(directory.path(), &["init", "--quiet", "-b", "main"]);
    git(
        directory.path(),
        &["config", "core.excludesFile", excludes.to_str().unwrap()],
    );

    std::fs::create_dir_all(directory.path().join("sub")).unwrap();
    std::fs::write(
        directory.path().join(".gitignore"),
        b"*.log\n.global_excludes\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("sub/.gitignore"), b"!keep.log\n").unwrap();
    std::fs::write(directory.path().join("root.log"), b"ignored\n").unwrap();
    std::fs::write(directory.path().join("sub/skip.log"), b"ignored\n").unwrap();
    std::fs::write(directory.path().join("sub/keep.log"), b"kept\n").unwrap();
    std::fs::write(directory.path().join("token.secret"), b"ignored\n").unwrap();
    std::fs::write(directory.path().join("staged_unborn.txt"), b"hello\n").unwrap();
    git(
        directory.path(),
        &["add", "staged_unborn.txt", ".gitignore", "sub/.gitignore"],
    );

    let expected_unborn = git(
        directory.path(),
        &[
            "-c",
            "diff.renames=false",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "-z",
        ],
    );
    let actual_unborn = LocalRepository::discover(directory.path())
        .unwrap()
        .status()
        .unwrap()
        .porcelain_v1(true);
    assert_eq!(actual_unborn, expected_unborn.stdout);
}

#[test]
fn status_matches_git_across_exec_symlinks_types_and_detached_head() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().unwrap();
    git(directory.path(), &["init", "--quiet", "-b", "main"]);

    let exec_path = directory.path().join("exec.sh");
    std::fs::write(&exec_path, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink("exec.sh", directory.path().join("link.lnk")).unwrap();
    std::fs::write(directory.path().join("swap.entry"), b"regular\n").unwrap();
    git(
        directory.path(),
        &["add", "exec.sh", "link.lnk", "swap.entry"],
    );
    git(
        directory.path(),
        &[
            "-c",
            "diff.renames=false",
            "commit",
            "--quiet",
            "-m",
            "baseline",
        ],
    );

    // Clean detached HEAD parity.
    git(
        directory.path(),
        &["checkout", "--quiet", "--detach", "HEAD"],
    );
    assert_eq!(
        LocalRepository::discover(directory.path())
            .unwrap()
            .status()
            .unwrap()
            .porcelain_v1(true),
        b""
    );

    // Executable flip, unstaged + staged type changes, detached HEAD, special byte paths.
    std::fs::set_permissions(&exec_path, std::fs::Permissions::from_mode(0o644)).unwrap();

    std::fs::remove_file(directory.path().join("swap.entry")).unwrap();
    symlink("exec.sh", directory.path().join("swap.entry")).unwrap();

    std::fs::remove_file(directory.path().join("link.lnk")).unwrap();
    std::fs::write(directory.path().join("link.lnk"), b"now a file\n").unwrap();
    git(directory.path(), &["add", "link.lnk"]);
    std::fs::remove_file(directory.path().join("link.lnk")).unwrap();
    symlink("other_target", directory.path().join("link.lnk")).unwrap();

    let weird_names = [
        OsString::from("with space.txt"),
        OsString::from("with\ttab.txt"),
        OsString::from("with\nnewline.txt"),
        OsString::from_vec(vec![b'r', b'a', b'w', 0xff, b'.', b'b', b'i', b'n']),
    ];
    for name in &weird_names {
        std::fs::write(directory.path().join(name), b"weird\n").unwrap();
    }
    git(directory.path(), &["add", "with space.txt"]);

    let expected = git(
        directory.path(),
        &[
            "-c",
            "diff.renames=false",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "-z",
        ],
    );
    let actual = LocalRepository::discover(directory.path())
        .unwrap()
        .status()
        .unwrap()
        .porcelain_v1(true);
    assert_eq!(actual, expected.stdout);
}
