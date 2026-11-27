#![cfg(unix)]

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use git2_core::commit;
use git2_core::log::{self, LogOptions, MAX_LOG_COUNT};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::stage;
use git2_core::transaction::Failpoint;
use git2_core::undo;

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
        .expect("Git must be installed for differential tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn init_adopted_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Ada Lovelace"]);
    git(dir.path(), &["config", "user.email", "ada@example.invalid"]);
    LocalRepository::discover(dir.path())
        .unwrap()
        .adopt(AdoptOptions::default())
        .unwrap();
    dir
}

#[test]
fn root_commit_and_nested_canonical_tree_sort_match_git() {
    let dir = init_adopted_repo();
    fs::write(dir.path().join("a-file"), b"dash\n").unwrap();
    fs::create_dir_all(dir.path().join("a/deep/nested")).unwrap();
    fs::write(dir.path().join("a/deep/nested/file"), b"tree\n").unwrap();
    fs::write(dir.path().join("a_file"), b"underscore\n").unwrap();

    let script = dir.path().join("run.sh");
    fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
    let mut perms = fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&script, perms).unwrap();

    symlink("../a-file", dir.path().join("a/link")).unwrap();
    let non_utf8 = OsString::from_vec(vec![b'n', b'o', b'n', 0xff]);
    fs::write(dir.path().join(&non_utf8), b"bytes\n").unwrap();

    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let expected_tree = String::from_utf8(git(dir.path(), &["write-tree"]).stdout).unwrap();

    let outcome = commit::apply(&repo, "initial snapshot").unwrap();
    assert_eq!(outcome.tree_oid.to_hex(), expected_tree.trim());

    git(dir.path(), &["fsck", "--full", "--strict"]);
    let head_oid = String::from_utf8(git(dir.path(), &["rev-parse", "HEAD"]).stdout).unwrap();
    assert_eq!(outcome.commit_oid.to_hex(), head_oid.trim());
    let head_tree =
        String::from_utf8(git(dir.path(), &["rev-parse", "HEAD^{tree}"]).stdout).unwrap();
    assert_eq!(outcome.tree_oid.to_hex(), head_tree.trim());

    let cat =
        String::from_utf8_lossy(&git(dir.path(), &["cat-file", "-p", "HEAD"]).stdout).into_owned();
    assert!(cat.contains("author Ada Lovelace <ada@example.invalid> "));
    assert!(cat.contains("committer Ada Lovelace <ada@example.invalid> "));
    assert!(cat.ends_with("\n\ninitial snapshot\n"));

    let log_entries = log::walk(&repo, LogOptions::default()).unwrap();
    assert_eq!(log_entries.len(), 1);
    assert_eq!(log_entries[0].oid, outcome.commit_oid);
    assert!(log_entries[0].commit.parents.is_empty());
    assert_eq!(log_entries[0].commit.message, "initial snapshot\n");
}

#[test]
fn follow_up_commits_multiline_normalization_and_empty_tree_deletion() {
    let dir = init_adopted_repo();
    fs::write(dir.path().join("tracked.txt"), b"v1\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c1 = commit::apply(&repo, "first\nwith newline\n").unwrap();

    fs::remove_file(dir.path().join("tracked.txt")).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c2 = commit::apply(&repo, "delete all tracked files").unwrap();
    assert_eq!(
        c2.tree_oid.to_hex(),
        "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
    );

    git(dir.path(), &["fsck", "--full", "--strict"]);
    let entries = log::walk(&repo, LogOptions::default()).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].oid, c2.commit_oid);
    assert_eq!(entries[0].commit.parents, vec![c1.commit_oid]);
    assert_eq!(entries[1].oid, c1.commit_oid);
    assert_eq!(entries[1].commit.message, "first\nwith newline\n");
}

#[test]
fn supports_detached_head_symbolic_branch_alias_and_packed_parent() {
    let dir = init_adopted_repo();
    fs::write(dir.path().join("base.txt"), b"1\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c1 = commit::apply(&repo, "root").unwrap();

    // Pack objects & refs to ensure commit resolves parent cleanly from packed state.
    git(dir.path(), &["gc", "--prune=now", "--quiet"]);
    git(dir.path(), &["pack-refs", "--all"]);
    let head_branch_ref = dir.path().join(".git/refs/heads/main");
    let alias_path = dir.path().join(".git/refs/heads/alias");
    fs::write(&alias_path, b"ref: refs/heads/main\n").unwrap();
    git(dir.path(), &["symbolic-ref", "HEAD", "refs/heads/alias"]);
    assert!(!head_branch_ref.exists());

    fs::write(dir.path().join("base.txt"), b"2\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c2 = commit::apply(&repo, "on symbolic alias with packed parent").unwrap();
    let leaf = fs::read_to_string(&head_branch_ref).unwrap();
    assert_eq!(leaf.trim(), c2.commit_oid.to_hex());

    // Now detach HEAD and make a third commit.
    git(dir.path(), &["checkout", "--quiet", "--detach", "HEAD"]);
    fs::write(dir.path().join("base.txt"), b"3\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c3 = commit::apply(&repo, "detached commit").unwrap();

    let head_contents = fs::read_to_string(dir.path().join(".git/HEAD")).unwrap();
    assert_eq!(head_contents.trim(), c3.commit_oid.to_hex());
    assert_eq!(
        fs::read_to_string(&head_branch_ref).unwrap().trim(),
        c2.commit_oid.to_hex()
    );

    let log_detached = log::walk(&repo, LogOptions::default()).unwrap();
    let oids: Vec<_> = log_detached.into_iter().map(|e| e.oid).collect();
    assert_eq!(oids, vec![c3.commit_oid, c2.commit_oid, c1.commit_oid]);
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn commit_refuses_blank_message_empty_delta_hooks_signing_ita_and_locks() {
    let dir = init_adopted_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();

    // Empty commit on unborn branch refused.
    assert!(commit::apply(&repo, "   \n\t").is_err());
    assert!(commit::apply(&repo, "a\0b").is_err());
    assert!(commit::apply(&repo, "empty unborn").is_err());

    fs::write(dir.path().join("f.txt"), b"v1\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    commit::apply(&repo, "root").unwrap();

    // Identical staged tree relative to HEAD refused.
    assert!(commit::apply(&repo, "nothing staged").is_err());

    // Intent-to-add entry refused.
    fs::write(dir.path().join("ita.txt"), b"pending\n").unwrap();
    git(dir.path(), &["add", "-N", "ita.txt"]);
    assert!(commit::apply(&repo, "with ita").is_err());
    git(dir.path(), &["reset", "--quiet", "--", "ita.txt"]);

    // Modify and stage for remaining precondition tests.
    fs::write(dir.path().join("f.txt"), b"v2\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();

    // Non-executable hooks are ignored (only executable hooks block).
    let default_hooks = dir.path().join(".git/hooks");
    fs::create_dir_all(&default_hooks).unwrap();
    let disabled = default_hooks.join("pre-commit");
    fs::write(&disabled, b"#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&disabled, fs::Permissions::from_mode(0o644)).unwrap();

    // Executable custom hook under core.hooksPath must refuse.
    let custom_hooks = dir.path().join(".githooks");
    fs::create_dir_all(&custom_hooks).unwrap();
    let custom_pre = custom_hooks.join("commit-msg");
    fs::write(&custom_pre, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&custom_pre, fs::Permissions::from_mode(0o755)).unwrap();
    git(dir.path(), &["config", "core.hooksPath", ".githooks"]);
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let err = commit::apply(&repo, "blocked by hook").unwrap_err();
    assert!(err.to_string().contains("commit-msg"));
    git(dir.path(), &["config", "--unset", "core.hooksPath"]);

    // commit.gpgSign = true must refuse.
    git(dir.path(), &["config", "commit.gpgSign", "true"]);
    let repo = LocalRepository::discover(dir.path()).unwrap();
    assert!(
        commit::apply(&repo, "gpg sign")
            .unwrap_err()
            .to_string()
            .contains("gpgSign")
    );
    git(dir.path(), &["config", "--unset", "commit.gpgSign"]);

    // Non-UTF-8 i18n.commitEncoding must refuse.
    git(dir.path(), &["config", "i18n.commitEncoding", "latin1"]);
    let repo = LocalRepository::discover(dir.path()).unwrap();
    assert!(
        commit::apply(&repo, "latin1")
            .unwrap_err()
            .to_string()
            .contains("commitEncoding")
    );
    git(dir.path(), &["config", "--unset", "i18n.commitEncoding"]);

    // Foreign ref lock must refuse before mutation.
    let ref_lock = dir.path().join(".git/refs/heads/main.lock");
    fs::write(&ref_lock, b"foreign lock\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    assert!(commit::apply(&repo, "locked branch").is_err());
    fs::remove_file(&ref_lock).unwrap();

    // Disabled pre-commit hook + clean config allows commit!
    let final_c = commit::apply(&repo, "commit succeeds cleanly").unwrap();
    assert_eq!(
        log::walk(&repo, LogOptions::default()).unwrap()[0].oid,
        final_c.commit_oid
    );
}

#[test]
fn undo_and_redo_work_for_root_and_normal_commits() {
    let dir = init_adopted_repo();
    fs::write(dir.path().join("a.txt"), b"v1\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();

    // Root commit.
    let root = commit::apply(&repo, "root commit").unwrap();
    assert!(dir.path().join(".git/refs/heads/main").exists());

    // Undo root commit -> returns branch to unborn, staged file stays in index.
    undo::apply(&repo).unwrap();
    assert!(!dir.path().join(".git/refs/heads/main").exists());
    assert!(log::walk(&repo, LogOptions::default()).is_err());

    // Undo the undo -> redoes exact root commit!
    undo::apply(&repo).unwrap();
    let entries = log::walk(&repo, LogOptions::default()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].oid, root.commit_oid);

    // Follow-up commit + undo + redo.
    fs::write(dir.path().join("a.txt"), b"v2\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let second = commit::apply(&repo, "second commit").unwrap();
    assert_eq!(log::walk(&repo, LogOptions::default()).unwrap().len(), 2);

    undo::apply(&repo).unwrap();
    let after_undo = log::walk(&repo, LogOptions::default()).unwrap();
    assert_eq!(after_undo.len(), 1);
    assert_eq!(after_undo[0].oid, root.commit_oid);

    undo::apply(&repo).unwrap();
    let after_redo = log::walk(&repo, LogOptions::default()).unwrap();
    assert_eq!(after_redo.len(), 2);
    assert_eq!(after_redo[0].oid, second.commit_oid);
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn log_bounds_and_fails_cleanly_on_corrupt_or_missing_parent() {
    let dir = init_adopted_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();

    // Unborn branch fails cleanly.
    assert!(log::walk(&repo, LogOptions::default()).is_err());
    assert!(
        log::walk(
            &repo,
            LogOptions {
                max_count: MAX_LOG_COUNT + 1,
            },
        )
        .is_err()
    );

    let mut commits = Vec::new();
    for i in 1..=5 {
        fs::write(dir.path().join("counter.txt"), format!("{i}\n")).unwrap();
        stage::apply(&repo, dir.path(), true, &[]).unwrap();
        commits.push(
            commit::apply(&repo, &format!("commit {i}"))
                .unwrap()
                .commit_oid,
        );
    }

    assert!(
        log::walk(&repo, LogOptions { max_count: 0 })
            .unwrap()
            .is_empty()
    );
    let two = log::walk(&repo, LogOptions { max_count: 2 }).unwrap();
    assert_eq!(two.len(), 2);
    assert_eq!(two[0].oid, commits[4]);
    assert_eq!(two[1].oid, commits[3]);

    // Delete parent commit[2] object from .git/objects -> must fail rather than truncate!
    let missing_hex = commits[2].to_hex();
    let obj_path = dir
        .path()
        .join(".git/objects")
        .join(&missing_hex[..2])
        .join(&missing_hex[2..]);
    fs::remove_file(&obj_path).unwrap();

    assert!(log::walk(&repo, LogOptions::default()).is_err());
    // Walking only 2 commits still works because it does not read commit[2].
    assert_eq!(
        log::walk(&repo, LogOptions { max_count: 2 }).unwrap().len(),
        2
    );
}

#[test]
fn io_failure_mid_commit_rolls_back_cleanly() {
    let dir = init_adopted_repo();
    fs::write(dir.path().join("a.txt"), b"v1\n").unwrap();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c1 = commit::apply(&repo, "base").unwrap();

    fs::write(dir.path().join("a.txt"), b"v2\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();

    let err = commit::apply_with_hook(&repo, "interrupted", |fp| match fp {
        Failpoint::BeforeAction(1) => Err(git2_core::Error::Io(std::io::Error::other(
            "injected io failure",
        ))),
        _ => Ok(()),
    })
    .unwrap_err();
    assert!(err.to_string().contains("injected io failure"));

    let entries = log::walk(&repo, LogOptions::default()).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].oid, c1.commit_oid);
    git(dir.path(), &["fsck", "--full", "--strict"]);
}
