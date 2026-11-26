#![cfg(unix)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use git2_core::recovery::RecoveryStore;
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::stage;
use git2_core::undo;

fn git_cmd(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "git2 test")
        .env("GIT_AUTHOR_EMAIL", "git2@example.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "git2 test")
        .env("GIT_COMMITTER_EMAIL", "git2@example.invalid")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn git_bytes(path: &Path, args: &[&OsStr]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "git2 test")
        .env("GIT_AUTHOR_EMAIL", "git2@example.invalid")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_NAME", "git2 test")
        .env("GIT_COMMITTER_EMAIL", "git2@example.invalid")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap()
}

fn git(path: &Path, args: &[&str]) -> Vec<u8> {
    let output = git_cmd(path, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn setup_twin_repos() -> (tempfile::TempDir, tempfile::TempDir) {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for dir in [a.path(), b.path()] {
        git(
            dir,
            &["init", "--quiet", "-b", "main", "--object-format=sha1"],
        );
        fs::write(dir.join("keep.txt"), b"initial\n").unwrap();
        fs::write(dir.join("modify.txt"), b"before mod\n").unwrap();
        fs::write(dir.join("delete_me.txt"), b"gone soon\n").unwrap();
        fs::write(dir.join("chmod.sh"), b"echo hi\n").unwrap();
        git(
            dir,
            &["add", "keep.txt", "modify.txt", "delete_me.txt", "chmod.sh"],
        );
        git(dir, &["commit", "-q", "-m", "base"]);

        fs::write(dir.join("modify.txt"), b"after mod\n").unwrap();
        fs::remove_file(dir.join("delete_me.txt")).unwrap();
        fs::write(dir.join("new.txt"), b"brand new\n").unwrap();
        fs::write(dir.join("empty.bin"), b"").unwrap();
        fs::write(dir.join("binary.bin"), b"\x00\x01\xff\r\n\x00").unwrap();
        let mut perms = fs::metadata(dir.join("chmod.sh")).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(dir.join("chmod.sh"), perms).unwrap();
        symlink("modify.txt", dir.join("sym.link")).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/nested.txt"), b"nested\n").unwrap();
        fs::write(dir.join("untracked_outside.txt"), b"not staged\n").unwrap();
    }
    (a, b)
}

#[test]
fn add_new_modified_deleted_empty_binary_exec_symlink_matches_git() {
    let (dir_git, dir_git2) = setup_twin_repos();
    let repo = LocalRepository::discover(dir_git2.path()).unwrap();
    let adopt = repo.adopt(AdoptOptions::default()).unwrap();

    let args = [
        "modify.txt",
        "delete_me.txt",
        "new.txt",
        "empty.bin",
        "binary.bin",
        "chmod.sh",
        "sym.link",
        "sub",
    ];
    let os_args: Vec<OsString> = args.iter().map(OsString::from).collect();

    git(
        dir_git.path(),
        &[
            "add",
            "--",
            "modify.txt",
            "delete_me.txt",
            "new.txt",
            "empty.bin",
            "binary.bin",
            "chmod.sh",
            "sym.link",
            "sub",
        ],
    );
    let outcome = stage::apply(&repo, dir_git2.path(), false, &os_args).unwrap();
    assert_ne!(outcome.operation_id, adopt.operation_id);

    assert_eq!(
        git(dir_git.path(), &["ls-files", "--stage", "-z"]),
        git(dir_git2.path(), &["ls-files", "--stage", "-z"])
    );
    assert_eq!(
        git(dir_git.path(), &["diff", "--cached", "--raw", "-z"]),
        git(dir_git2.path(), &["diff", "--cached", "--raw", "-z"])
    );
    git(dir_git2.path(), &["fsck", "--full"]);

    // Verify `git2 undo` restores the exact pre-add index
    undo::apply(&repo).unwrap();
    let undo_diff = git(dir_git2.path(), &["diff", "--cached", "--raw", "-z"]);
    assert!(undo_diff.is_empty(), "undo must clear newly staged changes");
    git(dir_git2.path(), &["fsck", "--full"]);
}

#[test]
fn add_literal_special_filenames_from_subdirectory() {
    let dir_git = tempfile::tempdir().unwrap();
    let dir_git2 = tempfile::tempdir().unwrap();

    let dash = OsStr::from_bytes(b"-flag-like");
    let brackets = OsStr::from_bytes(b"[bracket].txt");
    let star = OsStr::from_bytes(b"star*.txt");
    let nl = OsStr::from_bytes(b"line\nbreak.txt");
    let non_utf8 = OsStr::from_bytes(b"raw_\xff\xfe.bin");
    let wild_other = OsStr::from_bytes(b"star_extra.txt");

    for dir in [dir_git.path(), dir_git2.path()] {
        git(dir, &["init", "--quiet", "-b", "main"]);
        fs::write(dir.join("init.txt"), b"init\n").unwrap();
        git(dir, &["add", "init.txt"]);
        git(dir, &["commit", "-q", "-m", "init"]);

        let sub = dir.join("nested");
        fs::create_dir(&sub).unwrap();
        for (name, bytes) in [
            (dash, b"dash\n".as_slice()),
            (brackets, b"brackets\n"),
            (star, b"star\n"),
            (nl, b"newline\n"),
            (non_utf8, b"nonutf8\n"),
            (wild_other, b"must stay untracked\n"),
        ] {
            fs::write(sub.join(name), bytes).unwrap();
        }
    }

    let repo = LocalRepository::discover(dir_git2.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();

    // Ordinary Git treats `*` and `[` as pathspec globs unless `--literal-pathspecs` is set;
    // git2 add treats all arguments literally without globbing `star_extra.txt`!
    let mut git_args = vec![
        OsStr::new("--literal-pathspecs"),
        OsStr::new("add"),
        OsStr::new("--"),
    ];
    let literal = [dash, brackets, star, nl, non_utf8];
    git_args.extend(literal);
    let out = git_bytes(&dir_git.path().join("nested"), &git_args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let os_literal: Vec<OsString> = literal.iter().map(|s| s.to_os_string()).collect();
    stage::apply(&repo, &dir_git2.path().join("nested"), false, &os_literal).unwrap();

    assert_eq!(
        git(dir_git.path(), &["ls-files", "--stage", "-z"]),
        git(dir_git2.path(), &["ls-files", "--stage", "-z"])
    );
    git(dir_git2.path(), &["fsck", "--full"]);
}

#[test]
fn add_nested_gitignore_and_ignored_refusal() {
    let dir_git = tempfile::tempdir().unwrap();
    let dir_git2 = tempfile::tempdir().unwrap();
    for dir in [dir_git.path(), dir_git2.path()] {
        git(dir, &["init", "--quiet", "-b", "main"]);
        fs::write(dir.join(".gitignore"), b"*.log\n").unwrap();
        fs::create_dir(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/.gitignore"), b"!keep.log\n*.tmp\n").unwrap();
        fs::write(dir.join("root.log"), b"ignored root\n").unwrap();
        fs::write(dir.join("sub/drop.log"), b"ignored sub log\n").unwrap();
        fs::write(dir.join("sub/keep.log"), b"negated keep log\n").unwrap();
        fs::write(dir.join("sub/drop.tmp"), b"ignored tmp\n").unwrap();
        fs::create_dir(dir.join("ignored_dir")).unwrap();
        fs::write(dir.join("ignored_dir/file.txt"), b"inside\n").unwrap();
        fs::write(dir.join(".gitignore"), b"*.log\nignored_dir/\n").unwrap();
    }

    let repo = LocalRepository::discover(dir_git2.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();

    // Explicitly naming an ignored file or directory MUST refuse with zero changes
    assert!(stage::apply(&repo, dir_git2.path(), false, &[OsString::from("root.log")],).is_err());
    assert!(
        stage::apply(
            &repo,
            dir_git2.path(),
            false,
            &[OsString::from("ignored_dir")],
        )
        .is_err()
    );
    assert!(!dir_git2.path().join(".git/index").exists());

    // Directory add of `.gitignore` and `sub` matches `git add .` skipping ignored children
    git(dir_git.path(), &["add", "--all"]);
    stage::apply(&repo, dir_git2.path(), true, &[]).unwrap();
    assert_eq!(
        git(dir_git.path(), &["ls-files", "--stage", "-z"]),
        git(dir_git2.path(), &["ls-files", "--stage", "-z"])
    );
    git(dir_git2.path(), &["fsck", "--full"]);
}

#[test]
fn add_gitattributes_crlf_conversion_and_external_filter_refusal() {
    let dir_git = tempfile::tempdir().unwrap();
    let dir_git2 = tempfile::tempdir().unwrap();

    for dir in [dir_git.path(), dir_git2.path()] {
        git(dir, &["init", "--quiet", "-b", "main"]);
        git(dir, &["config", "core.autocrlf", "false"]);
        fs::write(
            dir.join(".gitattributes"),
            b"*.txt text\n*.crlf eol=lf\n*.bin binary\n",
        )
        .unwrap();
        fs::write(dir.join("doc.txt"), b"hello\r\nworld\r\n").unwrap();
        fs::write(dir.join("mixed.crlf"), b"a\r\nb\nc\r\n").unwrap();
        fs::write(dir.join("raw.bin"), b"keep\r\ncrlf\x00bytes\r\n").unwrap();
        git(dir, &["add", ".gitattributes"]);
        git(dir, &["commit", "-q", "-m", "attrs"]);
    }

    let repo = LocalRepository::discover(dir_git2.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();

    git(dir_git.path(), &["add", "doc.txt", "mixed.crlf", "raw.bin"]);
    stage::apply(
        &repo,
        dir_git2.path(),
        false,
        &[
            OsString::from("doc.txt"),
            OsString::from("mixed.crlf"),
            OsString::from("raw.bin"),
        ],
    )
    .unwrap();

    assert_eq!(
        git(dir_git.path(), &["ls-files", "--stage", "-z"]),
        git(dir_git2.path(), &["ls-files", "--stage", "-z"])
    );
    git(dir_git2.path(), &["fsck", "--full"]);

    // Active external filter MUST be refused WITHOUT running the external command or mutating index
    let marker = dir_git2.path().join("clean_ran_marker");
    let clean_cmd = format!("touch {}; cat", marker.display());
    git(dir_git2.path(), &["config", "filter.ext.clean", &clean_cmd]);
    fs::write(
        dir_git2.path().join(".gitattributes"),
        b"*.filt filter=ext\n",
    )
    .unwrap();
    fs::write(dir_git2.path().join("secret.filt"), b"trigger filter\n").unwrap();
    fs::write(
        dir_git2.path().join("plain.dat"),
        b"no external filter here\n",
    )
    .unwrap();

    let repo_reopened = LocalRepository::discover(dir_git2.path()).unwrap();
    let index_before = fs::read(dir_git2.path().join(".git/index")).unwrap();
    let err = stage::apply(
        &repo_reopened,
        dir_git2.path(),
        false,
        &[OsString::from("secret.filt")],
    )
    .unwrap_err();
    assert!(
        err.to_string()
            .contains("external filters are not supported"),
        "expected external filter refusal, got: {err}"
    );
    assert!(
        !marker.exists(),
        "git2 must never execute external filter clean commands"
    );
    assert_eq!(
        fs::read(dir_git2.path().join(".git/index")).unwrap(),
        index_before
    );

    // Non-matching path in the same repo STILL stages cleanly!
    stage::apply(
        &repo_reopened,
        dir_git2.path(),
        false,
        &[OsString::from("plain.dat")],
    )
    .unwrap();
    assert!(
        String::from_utf8(git(dir_git2.path(), &["ls-files"]))
            .unwrap()
            .contains("plain.dat")
    );
}

#[test]
fn add_all_unborn_detached_and_foreign_lock_refusal() {
    // 1. Unborn branch: no `.git/index` initially
    let unborn = tempfile::tempdir().unwrap();
    git(unborn.path(), &["init", "--quiet", "-b", "main"]);
    fs::write(unborn.path().join("first.txt"), b"first\n").unwrap();
    let repo_unborn = LocalRepository::discover(unborn.path()).unwrap();
    repo_unborn.adopt(AdoptOptions::default()).unwrap();
    assert!(!unborn.path().join(".git/index").exists());

    stage::apply(&repo_unborn, unborn.path(), true, &[]).unwrap();
    assert!(unborn.path().join(".git/index").is_file());
    assert!(
        String::from_utf8(git(unborn.path(), &["diff", "--cached", "--name-only"]))
            .unwrap()
            .contains("first.txt")
    );
    git(unborn.path(), &["fsck", "--full"]);

    // 2. Detached HEAD + pre-existing foreign `.git/index.lock`
    git(unborn.path(), &["commit", "-q", "-m", "initial"]);
    let head = String::from_utf8(git(unborn.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();
    git(unborn.path(), &["checkout", "-q", "--detach", &head]);
    let detached = LocalRepository::discover(unborn.path()).unwrap();
    fs::write(unborn.path().join("second.txt"), b"second\n").unwrap();

    let lock_path = unborn.path().join(".git/index.lock");
    let sentinel = b"foreign-index-lock";
    fs::write(&lock_path, sentinel).unwrap();
    let err = stage::apply(
        &detached,
        unborn.path(),
        false,
        &[OsString::from("second.txt")],
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown Git lock"), "{err}");
    assert_eq!(fs::read(&lock_path).unwrap(), sentinel);
    fs::remove_file(&lock_path).unwrap();

    stage::apply(
        &detached,
        unborn.path(),
        false,
        &[OsString::from("second.txt")],
    )
    .unwrap();
    let store = RecoveryStore::open(detached.git_dir()).unwrap();
    assert!(!store.has_pending().unwrap());
    git(unborn.path(), &["fsck", "--full"]);
}
