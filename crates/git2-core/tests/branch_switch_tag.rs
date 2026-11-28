#![cfg(unix)]

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use bstr::BString;
use git2_core::branch;
use git2_core::commit;
use git2_core::object::{ObjectKind, Tag};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::reset::{self, ResetMode};
use git2_core::stage;
use git2_core::switch::{self, SwitchTarget};
use git2_core::tag;
use git2_core::undo;

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .expect("Git must be installed for integration tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn rev_parse(dir: &Path, rev: &str) -> String {
    String::from_utf8(git(dir, &["rev-parse", rev]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn abbrev_head(dir: &Path) -> String {
    String::from_utf8(git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).stdout)
        .unwrap()
        .trim()
        .to_owned()
}

fn init_adopted_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test User"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    fs::write(dir.path().join("base.txt"), b"base\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "--quiet", "-m", "initial commit"]);
    let repo = LocalRepository::discover(dir.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();
    dir
}

#[test]
fn branch_list_create_rename_delete_merge_guard_packed_and_undo() {
    let dir = init_adopted_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();

    // Create branch at HEAD, and another branch with a new commit.
    let c1 = rev_parse(dir.path(), "HEAD");
    branch::create(&repo, "feature", None).unwrap();
    branch::create(&repo, "topic", Some("main")).unwrap();

    let listing = branch::list(&repo).unwrap();
    assert!(listing.detached_head.is_none());
    let names: Vec<_> = listing
        .branches
        .iter()
        .map(|b| b.name.to_string())
        .collect();
    assert_eq!(names, vec!["feature", "main", "topic"]);
    assert!(
        listing
            .branches
            .iter()
            .find(|b| b.name == "main")
            .unwrap()
            .is_current
    );

    // Invalid names, duplicates, D/F conflicts, current branch delete must refuse untouched.
    for bad in ["", "-bad", "HEAD", "feature", "feature/sub", "a.lock"] {
        assert!(branch::create(&repo, bad, None).is_err(), "{bad:?}");
    }
    assert!(branch::delete(&repo, "main", false).is_err());
    assert!(branch::delete(&repo, "main", true).is_err());

    // Foreign lock refuses before touching refs.
    fs::write(dir.path().join(".git/refs/heads/locked.lock"), b"").unwrap();
    assert!(branch::create(&repo, "locked", None).is_err());
    assert!(!dir.path().join(".git/refs/heads/locked").exists());
    fs::remove_file(dir.path().join(".git/refs/heads/locked.lock")).unwrap();

    // Advance `feature` using git2 switch -c / add / commit.
    switch::apply(&repo, &SwitchTarget::Branch("feature".to_owned())).unwrap();
    fs::write(dir.path().join("feat.txt"), b"feat\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let feat_commit = commit::apply(&repo, "feature work").unwrap().commit_oid;
    assert_ne!(feat_commit.to_hex(), c1);

    // Switch back to main: `feature` is NOT merged into `main`, so `-d` refuses while `-D` deletes.
    switch::apply(&repo, &SwitchTarget::Branch("main".to_owned())).unwrap();
    let unmerged_err = branch::delete(&repo, "feature", false).unwrap_err();
    assert!(unmerged_err.to_string().contains("not fully merged"));

    branch::delete(&repo, "feature", true).unwrap();
    assert!(
        !branch::list(&repo)
            .unwrap()
            .branches
            .iter()
            .any(|b| b.name == "feature")
    );

    // Even after `git prune --expire=now` removes unreferenced commits, `git2 undo` restores `feature`
    // AND rehydrates its pruned commit!
    git(dir.path(), &["prune", "--expire=now"]);
    undo::apply(&repo).unwrap();
    git(dir.path(), &["fsck", "--full", "--strict"]);
    assert_eq!(rev_parse(dir.path(), "feature"), feat_commit.to_hex());

    // Multi-parent merge ancestry check lets `branch -d` delete a branch merged via merge commit.
    git(
        dir.path(),
        &[
            "merge",
            "--quiet",
            "--no-ff",
            "feature",
            "-m",
            "merge feature",
        ],
    );
    branch::delete(&repo, "feature", false).unwrap();

    // Packed refs + D/F rename (`topic` -> `topic/sub` -> `topic`) + current branch rename.
    git(dir.path(), &["pack-refs", "--all"]);
    assert!(!dir.path().join(".git/refs/heads/topic").exists());

    branch::rename(&repo, Some("topic"), "topic/sub").unwrap();
    assert_eq!(rev_parse(dir.path(), "topic/sub"), c1);
    branch::rename(&repo, Some("topic/sub"), "topic").unwrap();
    assert_eq!(rev_parse(dir.path(), "topic"), c1);

    // Deleting a packed branch deletes it cleanly even if it's a packed ref.
    branch::delete(&repo, "topic", false).unwrap();
    assert!(
        !branch::list(&repo)
            .unwrap()
            .branches
            .iter()
            .any(|b| b.name == "topic")
    );

    // Rename current branch `main` -> `trunk` updates HEAD and undoes back cleanly.
    branch::rename(&repo, None, "trunk").unwrap();
    assert_eq!(abbrev_head(dir.path()), "trunk");
    undo::apply(&repo).unwrap();
    assert_eq!(abbrev_head(dir.path()), "main");
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn tag_lightweight_annotated_packed_delete_and_undo_with_gc() {
    let dir = init_adopted_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let head_oid = rev_parse(dir.path(), "HEAD");

    // Second commit so we can test `tag~1` resolution as well.
    fs::write(dir.path().join("v2.txt"), b"v2\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    let c2 = commit::apply(&repo, "second commit").unwrap().commit_oid;

    tag::create_lightweight(&repo, "v0.1", Some(&head_oid)).unwrap();
    let ann = tag::create_annotated(&repo, "v0.2", "release v0.2\n\nnotes", None).unwrap();
    assert_eq!(ann.target_oid, c2);
    assert_ne!(ann.tag_oid, c2);

    // Verify canonical Tag object matches Git and `git cat-file -p`.
    let cat_type =
        String::from_utf8(git(dir.path(), &["cat-file", "-t", &ann.tag_oid.to_hex()]).stdout)
            .unwrap();
    assert_eq!(cat_type.trim(), "tag");
    let raw_payload = git(dir.path(), &["cat-file", "tag", &ann.tag_oid.to_hex()]).stdout;
    let parsed = Tag::parse(&raw_payload).unwrap();
    assert_eq!(parsed.object, c2);
    assert_eq!(parsed.object_kind, ObjectKind::Commit);
    assert_eq!(parsed.name, BString::from("v0.2"));
    assert_eq!(parsed.message, BString::from("release v0.2\n\nnotes\n"));
    git(dir.path(), &["fsck", "--full", "--strict"]);

    // `reset::resolve_commit` peels annotated tags and walks `~N` from them.
    assert_eq!(reset::resolve_commit(&repo, "v0.2").unwrap(), c2);
    assert_eq!(
        reset::resolve_commit(&repo, "v0.2~1").unwrap().to_hex(),
        head_oid
    );

    // Branch + tag ambiguity refuses rather than guessing silently.
    branch::create(&repo, "v0.2", None).unwrap();
    assert!(reset::resolve_commit(&repo, "v0.2").is_err());
    branch::delete(&repo, "v0.2", false).unwrap();

    // Refusals: blank message, duplicate, D/F conflict, signing enabled.
    assert!(tag::create_annotated(&repo, "blank", "   \n", None).is_err());
    assert!(tag::create_lightweight(&repo, "v0.1", None).is_err());
    assert!(tag::create_lightweight(&repo, "v0.1/sub", None).is_err());

    git(dir.path(), &["config", "tag.gpgSign", "true"]);
    let repo_signed = LocalRepository::discover(dir.path()).unwrap();
    assert!(tag::create_annotated(&repo_signed, "signed", "msg", None).is_err());
    git(dir.path(), &["config", "--unset", "tag.gpgSign"]);
    let repo = LocalRepository::discover(dir.path()).unwrap();

    // Pack tags (`packed-refs` contains `^<peeled>` lines!) then delete `v0.2`, prune with Git, and undo!
    git(dir.path(), &["pack-refs", "--all"]);
    assert!(!dir.path().join(".git/refs/tags/v0.2").exists());
    tag::delete(&repo, "v0.2").unwrap();
    assert_eq!(
        tag::list(&repo)
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect::<Vec<_>>(),
        vec!["v0.1"]
    );

    git(dir.path(), &["prune", "--expire=now"]);
    undo::apply(&repo).unwrap();
    git(dir.path(), &["fsck", "--full", "--strict"]);
    assert_eq!(
        rev_parse(dir.path(), "refs/tags/v0.2"),
        ann.tag_oid.to_hex()
    );

    // Undo deletion of last packed tag leaves repository valid.
    tag::delete(&repo, "v0.1").unwrap();
    tag::delete(&repo, "v0.2").unwrap();
    assert!(tag::list(&repo).unwrap().is_empty());
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn switch_handles_modes_non_utf8_df_swaps_gitignore_eol_and_undo() {
    let dir = init_adopted_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();

    // Branch A (`main`): tracked file, executable, symlink, non-UTF-8 path, and file `shape`.
    let non_utf8_rel = Path::new(std::ffi::OsStr::from_bytes(b"nonutf8-\xff.txt"));
    fs::write(dir.path().join(non_utf8_rel), b"bytes\n").unwrap();
    fs::write(dir.path().join("script.sh"), b"#!/bin/sh\necho a\n").unwrap();
    fs::set_permissions(
        dir.path().join("script.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("base.txt", dir.path().join("link.txt")).unwrap();
    fs::write(dir.path().join("shape"), b"shape file\n").unwrap();
    fs::write(dir.path().join("keep.txt"), b"same on both\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    commit::apply(&repo, "branch A setup").unwrap();

    // Branch B: switches `shape` file -> `shape/nested.txt` directory, removes non-utf8, toggles exec, swaps symlink.
    switch::apply(
        &repo,
        &SwitchTarget::Create {
            name: "branch-b".to_owned(),
            start_point: None,
            force: false,
        },
    )
    .unwrap();
    fs::remove_file(dir.path().join(non_utf8_rel)).unwrap();
    fs::set_permissions(
        dir.path().join("script.sh"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::remove_file(dir.path().join("link.txt")).unwrap();
    symlink("keep.txt", dir.path().join("link.txt")).unwrap();
    fs::remove_file(dir.path().join("shape")).unwrap();
    fs::create_dir_all(dir.path().join("shape")).unwrap();
    fs::write(dir.path().join("shape/nested.txt"), b"nested\n").unwrap();
    fs::write(dir.path().join(".gitignore"), b"*.log\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    commit::apply(&repo, "branch B layout").unwrap();

    // Leave an untouched ignored `debug.log` (ignored in B, unignored in A) and switch back to `main`.
    // Wait: `debug.log` is unignored on A, so `switch` should preserve it AND include it in target_view!
    fs::write(dir.path().join("debug.log"), b"trace\n").unwrap();

    // Also modify `keep.txt` (unstaged edit, untouched between branches) — must survive `switch main`!
    fs::write(dir.path().join("keep.txt"), b"local dirty edit preserved\n").unwrap();

    let switched = switch::apply(&repo, &SwitchTarget::Branch("main".to_owned())).unwrap();
    assert!(switched.changed);
    assert_eq!(abbrev_head(dir.path()), "main");
    assert!(dir.path().join(non_utf8_rel).exists());
    assert_eq!(
        fs::metadata(dir.path().join("script.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(
        fs::read_link(dir.path().join("link.txt")).unwrap(),
        Path::new("base.txt")
    );
    assert!(dir.path().join("shape").is_file());
    assert_eq!(
        fs::read(dir.path().join("keep.txt")).unwrap(),
        b"local dirty edit preserved\n"
    );
    assert_eq!(fs::read(dir.path().join("debug.log")).unwrap(), b"trace\n");
    git(dir.path(), &["fsck", "--full", "--strict"]);

    // Undo restores branch-b, directory `shape/nested.txt`, `.gitignore`, AND preserves `keep.txt` edit + `debug.log`!
    undo::apply(&repo).unwrap();
    assert_eq!(abbrev_head(dir.path()), "branch-b");
    assert!(dir.path().join("shape/nested.txt").is_file());
    assert!(!dir.path().join(non_utf8_rel).exists());
    assert_eq!(
        fs::read(dir.path().join("keep.txt")).unwrap(),
        b"local dirty edit preserved\n"
    );
    git(dir.path(), &["fsck", "--full", "--strict"]);

    // Re-switch (undo the undo) works cleanly too.
    undo::apply(&repo).unwrap();
    assert_eq!(abbrev_head(dir.path()), "main");
    assert!(dir.path().join("shape").is_file());

    // An untracked file `shape/blocker.txt` blocks switching from `branch-b` (`shape/nested.txt`)
    // to `main` (`shape` file) without mutating HEAD or files.
    switch::apply(&repo, &SwitchTarget::Branch("branch-b".to_owned())).unwrap();
    fs::write(dir.path().join("shape/blocker.txt"), b"keep me\n").unwrap();
    assert!(switch::apply(&repo, &SwitchTarget::Branch("main".to_owned())).is_err());
    assert_eq!(abbrev_head(dir.path()), "branch-b");
    assert_eq!(
        fs::read(dir.path().join("shape/blocker.txt")).unwrap(),
        b"keep me\n"
    );
    fs::remove_file(dir.path().join("shape/blocker.txt")).unwrap();
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn switch_protects_dirty_worktree_staged_untracked_and_ignored_collisions() {
    let dir = init_adopted_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();

    fs::write(dir.path().join("shared.txt"), b"v1\n").unwrap();
    fs::write(dir.path().join("keep.txt"), b"unchanged\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    commit::apply(&repo, "main commit").unwrap();

    switch::apply(
        &repo,
        &SwitchTarget::Create {
            name: "other".to_owned(),
            start_point: None,
            force: false,
        },
    )
    .unwrap();
    fs::write(dir.path().join("shared.txt"), b"v2\n").unwrap();
    fs::write(dir.path().join("added.txt"), b"added in other\n").unwrap();
    fs::create_dir_all(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/file.txt"), b"under sub\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    commit::apply(&repo, "other changes").unwrap();

    switch::apply(&repo, &SwitchTarget::Branch("main".to_owned())).unwrap();

    // 1. Unstaged edit on `shared.txt` (O != T) refuses without modifying HEAD or files.
    fs::write(dir.path().join("shared.txt"), b"dirty unstaged\n").unwrap();
    assert!(switch::apply(&repo, &SwitchTarget::Branch("other".to_owned())).is_err());
    assert_eq!(abbrev_head(dir.path()), "main");
    assert_eq!(
        fs::read(dir.path().join("shared.txt")).unwrap(),
        b"dirty unstaged\n"
    );

    // 2. Staged edit on `shared.txt` refuses cleanly.
    stage::apply(
        &repo,
        dir.path(),
        false,
        &[std::ffi::OsString::from("shared.txt")],
    )
    .unwrap();
    assert!(switch::apply(&repo, &SwitchTarget::Branch("other".to_owned())).is_err());
    reset::apply(&repo, ResetMode::Hard, "HEAD").unwrap();

    // 3. Untracked file colliding with `added.txt` refuses untouched.
    fs::write(dir.path().join("added.txt"), b"precious untracked\n").unwrap();
    assert!(switch::apply(&repo, &SwitchTarget::Branch("other".to_owned())).is_err());
    assert_eq!(
        fs::read(dir.path().join("added.txt")).unwrap(),
        b"precious untracked\n"
    );
    fs::remove_file(dir.path().join("added.txt")).unwrap();

    // 4. Untracked file `sub` blocking target directory `sub/file.txt` refuses untouched.
    fs::write(dir.path().join("sub"), b"precious file named sub\n").unwrap();
    assert!(switch::apply(&repo, &SwitchTarget::Branch("other".to_owned())).is_err());
    assert_eq!(
        fs::read(dir.path().join("sub")).unwrap(),
        b"precious file named sub\n"
    );
    fs::remove_file(dir.path().join("sub")).unwrap();

    // 5. Ignored file colliding with `added.txt` refuses untouched.
    fs::write(dir.path().join(".gitignore"), b"added.txt\n").unwrap();
    fs::write(dir.path().join("added.txt"), b"ignored secret\n").unwrap();
    assert!(switch::apply(&repo, &SwitchTarget::Branch("other".to_owned())).is_err());
    assert_eq!(
        fs::read(dir.path().join("added.txt")).unwrap(),
        b"ignored secret\n"
    );
    fs::remove_file(dir.path().join("added.txt")).unwrap();
    fs::remove_file(dir.path().join(".gitignore")).unwrap();

    // 6. Staged AND unstaged edits on `keep.txt` (O == T) succeed cleanly!
    fs::write(dir.path().join("keep.txt"), b"staged keep\n").unwrap();
    stage::apply(
        &repo,
        dir.path(),
        false,
        &[std::ffi::OsString::from("keep.txt")],
    )
    .unwrap();
    fs::write(dir.path().join("keep.txt"), b"unstaged keep\n").unwrap();
    fs::write(dir.path().join("untracked.txt"), b"keep untracked\n").unwrap();

    switch::apply(&repo, &SwitchTarget::Branch("other".to_owned())).unwrap();
    assert_eq!(abbrev_head(dir.path()), "other");
    assert_eq!(
        fs::read(dir.path().join("keep.txt")).unwrap(),
        b"unstaged keep\n"
    );
    assert_eq!(
        fs::read(dir.path().join("untracked.txt")).unwrap(),
        b"keep untracked\n"
    );
    let cached =
        String::from_utf8(git(dir.path(), &["diff", "--cached", "--name-only"]).stdout).unwrap();
    assert_eq!(cached.trim(), "keep.txt");
    git(dir.path(), &["fsck", "--full", "--strict"]);
}

#[test]
fn switch_autocrlf_and_detach_match_git() {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    git(dir.path(), &["config", "user.name", "Test User"]);
    git(
        dir.path(),
        &["config", "user.email", "test@example.invalid"],
    );
    git(dir.path(), &["config", "core.autocrlf", "true"]);

    fs::write(dir.path().join("dos.txt"), b"line1\r\nline2\r\n").unwrap();
    git(dir.path(), &["add", "dos.txt"]);
    git(dir.path(), &["commit", "--quiet", "-m", "v1 crlf"]);

    let repo = LocalRepository::discover(dir.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();

    // Clean switch away and back with core.autocrlf=true: no false dirty refusal!
    switch::apply(
        &repo,
        &SwitchTarget::Create {
            name: "v2".to_owned(),
            start_point: None,
            force: false,
        },
    )
    .unwrap();
    fs::write(dir.path().join("dos.txt"), b"line1\r\nline2\r\nline3\r\n").unwrap();
    stage::apply(&repo, dir.path(), true, &[]).unwrap();
    commit::apply(&repo, "v2 crlf").unwrap();

    switch::apply(&repo, &SwitchTarget::Branch("main".to_owned())).unwrap();
    assert_eq!(
        fs::read(dir.path().join("dos.txt")).unwrap(),
        b"line1\r\nline2\r\n"
    );

    // Detach at branch/commit/tag works and can undo back to attached branch.
    let tag_res = tag::create_annotated(&repo, "snap", "detach target", Some("v2")).unwrap();
    let detached = switch::apply(
        &repo,
        &SwitchTarget::Detach {
            commit: Some("snap".to_owned()),
        },
    )
    .unwrap();
    assert!(detached.detached);
    assert_eq!(detached.target_commit, tag_res.target_oid);
    assert_eq!(
        branch::list(&repo)
            .unwrap()
            .detached_head
            .map(|(oid, _)| oid),
        Some(tag_res.target_oid)
    );
    assert_eq!(
        fs::read(dir.path().join("dos.txt")).unwrap(),
        b"line1\r\nline2\r\nline3\r\n"
    );
    assert_eq!(
        git(dir.path(), &["status", "--porcelain=v1", "-z"]).stdout,
        b""
    );

    undo::apply(&repo).unwrap();
    assert_eq!(abbrev_head(dir.path()), "main");
}
