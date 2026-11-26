#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use bstr::BString;
use git2_core::object::{
    Commit, GitOid, ObjectKind, RawObject, Signature, Tree, TreeEntry, TreeMode,
};
use git2_core::recovery::{ContentId, Operation, OperationKind, RecoveryStore};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::reset::{self, MAX_FIRST_PARENT_STEPS, ResetMode};
use git2_core::store::ObjectStore;
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

fn init_history_repo(root: &Path) -> (String, String, String) {
    git(
        root,
        &["init", "--quiet", "-b", "main", "--object-format=sha1"],
    );
    fs::write(root.join("a.txt"), b"v1 a\n").unwrap();
    fs::write(root.join("b.txt"), b"v1 b\n").unwrap();
    git(root, &["add", "a.txt", "b.txt"]);
    git(root, &["commit", "-q", "-m", "c1"]);
    let c1 = String::from_utf8(git(root, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();

    git(root, &["branch", "side"]);

    fs::write(root.join("a.txt"), b"v2 a\n").unwrap();
    let exec_path = root.join("run.sh");
    fs::write(&exec_path, b"#!/bin/sh\necho 2\n").unwrap();
    let mut perms = fs::metadata(&exec_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&exec_path, perms).unwrap();
    git(root, &["add", "a.txt", "run.sh"]);
    git(root, &["commit", "-q", "-m", "c2"]);
    let c2 = String::from_utf8(git(root, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();

    git(root, &["rm", "-q", "b.txt"]);
    fs::write(root.join("c.txt"), b"v3 c\n").unwrap();
    git(root, &["add", "c.txt"]);
    git(root, &["commit", "-q", "-m", "c3"]);
    let c3 = String::from_utf8(git(root, &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();

    (c1, c2, c3)
}

fn snapshot_worktree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else if meta.file_type().is_symlink() {
                out.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read_link(&path)
                        .unwrap()
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                ));
            } else {
                out.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn oplog_ops(store: &RecoveryStore) -> Vec<(ContentId, Operation)> {
    let mut out = Vec::new();
    let mut cur = Some(store.head().unwrap());
    while let Some(id) = cur {
        let op = store.read_operation(id).unwrap();
        cur = op.parent;
        out.push((id, op));
    }
    out
}

fn dirty_staged_and_worktree(root: &Path) {
    fs::write(root.join("a.txt"), b"staged a\n").unwrap();
    git(root, &["add", "a.txt"]);
    fs::write(root.join("a.txt"), b"dirty worktree a\n").unwrap();
    fs::write(root.join("extra_untracked.txt"), b"keep untracked\n").unwrap();
}

#[test]
fn reset_soft_mixed_hard_match_git_on_supported_matrix() {
    for (mode, git_flag, rev) in [
        (ResetMode::Soft, "--soft", "HEAD~1"),
        (ResetMode::Mixed, "--mixed", "HEAD~2"),
        (ResetMode::Hard, "--hard", "side"),
        (ResetMode::Mixed, "--mixed", "main~1"),
    ] {
        let dir_git = tempfile::tempdir().unwrap();
        let dir_git2 = tempfile::tempdir().unwrap();
        let (c1_a, c2_a, _) = init_history_repo(dir_git.path());
        let (c1_b, c2_b, _) = init_history_repo(dir_git2.path());
        assert_eq!(c1_a, c1_b);
        assert_eq!(c2_a, c2_b);

        dirty_staged_and_worktree(dir_git.path());
        dirty_staged_and_worktree(dir_git2.path());

        let repo = LocalRepository::discover(dir_git2.path()).unwrap();
        repo.adopt(AdoptOptions::default()).unwrap();
        let head_log_before = fs::read(dir_git2.path().join(".git/logs/HEAD")).ok();
        let branch_log_before = fs::read(dir_git2.path().join(".git/logs/refs/heads/main")).ok();

        git(dir_git.path(), &["reset", "-q", git_flag, rev]);
        let outcome = reset::apply(&repo, mode, rev).unwrap();

        assert_eq!(
            git(dir_git.path(), &["rev-parse", "HEAD"]),
            git(dir_git2.path(), &["rev-parse", "HEAD"])
        );
        assert_eq!(
            outcome.target_commit.to_hex(),
            String::from_utf8(git(dir_git2.path(), &["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
        );
        assert_eq!(
            git(dir_git.path(), &["ls-files", "--stage", "-z"]),
            git(dir_git2.path(), &["ls-files", "--stage", "-z"])
        );
        assert_eq!(
            git(dir_git.path(), &["status", "--porcelain=v1", "-z"]),
            git(dir_git2.path(), &["status", "--porcelain=v1", "-z"])
        );
        assert_eq!(
            snapshot_worktree(dir_git.path()),
            snapshot_worktree(dir_git2.path())
        );
        assert!(
            !dir_git2.path().join(".git/ORIG_HEAD").exists(),
            "v0.1 reset must leave ORIG_HEAD unchanged"
        );
        assert_eq!(
            fs::read(dir_git2.path().join(".git/logs/HEAD")).ok(),
            head_log_before,
            "v0.1 reset must leave HEAD reflog unchanged"
        );
        assert_eq!(
            fs::read(dir_git2.path().join(".git/logs/refs/heads/main")).ok(),
            branch_log_before,
            "v0.1 reset must leave branch reflog unchanged"
        );
        git(dir_git2.path(), &["fsck", "--full"]);
    }

    // Detached HEAD + explicit 40-hex commit ID reset
    let dir = tempfile::tempdir().unwrap();
    let (c1, _, c3) = init_history_repo(dir.path());
    git(dir.path(), &["checkout", "-q", "--detach", &c3]);
    let repo = LocalRepository::discover(dir.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();
    let res = reset::apply(&repo, ResetMode::Hard, &c1).unwrap();
    assert_eq!(res.target_commit.to_hex(), c1);
    assert_eq!(
        fs::read_to_string(dir.path().join(".git/HEAD"))
            .unwrap()
            .trim(),
        c1
    );
    git(dir.path(), &["fsck", "--full"]);
}

#[test]
fn reset_hard_untracked_collision_and_ignored_obstruction() {
    let dir = init_repo();
    fs::write(
        dir.path().join(".gitignore"),
        b"*.ignored\nobst/ignored.txt\n",
    )
    .unwrap();
    fs::write(dir.path().join("target.txt"), b"from commit\n").unwrap();
    fs::create_dir_all(dir.path().join("obst")).unwrap();
    fs::write(dir.path().join("obst/file"), b"was file\n").unwrap();
    git(
        dir.path(),
        &["add", ".gitignore", "target.txt", "obst/file"],
    );
    git(dir.path(), &["commit", "-q", "-m", "v1"]);
    let v1 = String::from_utf8(git(dir.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();

    // Move to v2 where `target.txt` does NOT exist
    git(dir.path(), &["rm", "-q", "target.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "v2"]);

    let repo = LocalRepository::discover(dir.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();

    // Option A: colliding non-ignored untracked file IS overwritten cleanly and recoverable via undo!
    fs::write(dir.path().join("target.txt"), b"untracked collider bytes\n").unwrap();
    fs::write(dir.path().join("unrelated.txt"), b"unrelated stays\n").unwrap();
    reset::apply(&repo, ResetMode::Hard, &v1).unwrap();
    assert_eq!(
        fs::read(dir.path().join("target.txt")).unwrap(),
        b"from commit\n"
    );
    assert_eq!(
        fs::read(dir.path().join("unrelated.txt")).unwrap(),
        b"unrelated stays\n"
    );

    undo::apply(&repo).unwrap();
    assert_eq!(
        fs::read(dir.path().join("target.txt")).unwrap(),
        b"untracked collider bytes\n"
    );

    // Ignored obstruction on a path replaced by a commit file MUST refuse without writes
    fs::write(dir.path().join(".gitignore"), b"target.txt\n*.ignored\n").unwrap();
    fs::write(dir.path().join("target.txt"), b"precious ignored file\n").unwrap();
    let head_before = fs::read(dir.path().join(".git/refs/heads/main")).unwrap();
    let err = reset::apply(&repo, ResetMode::Hard, &v1).unwrap_err();
    assert!(
        err.to_string().contains("ignored entry"),
        "expected ignored obstruction refusal, got: {err}"
    );
    assert_eq!(
        fs::read(dir.path().join("target.txt")).unwrap(),
        b"precious ignored file\n"
    );
    assert_eq!(
        fs::read(dir.path().join(".git/refs/heads/main")).unwrap(),
        head_before
    );
}

#[test]
fn reset_refuses_invalid_revisions_gitlinks_and_unborn_head() {
    // 1. Unborn HEAD
    let unborn = tempfile::tempdir().unwrap();
    git(unborn.path(), &["init", "--quiet", "-b", "main"]);
    let repo_unborn = LocalRepository::discover(unborn.path()).unwrap();
    repo_unborn.adopt(AdoptOptions::default()).unwrap();
    assert!(reset::resolve_commit(&repo_unborn, "HEAD").is_err());

    // 2. Invalid syntax / missing ancestors / non-commit OID
    let dir = tempfile::tempdir().unwrap();
    init_history_repo(dir.path());
    let repo = LocalRepository::discover(dir.path()).unwrap();
    repo.adopt(AdoptOptions::default()).unwrap();
    for bad in [
        "",
        "HEAD^",
        "HEAD@{0}",
        "side..main",
        "HEAD~99",
        "nonexistent-branch",
        &format!("HEAD~{}", MAX_FIRST_PARENT_STEPS + 1),
    ] {
        assert!(
            reset::resolve_commit(&repo, bad).is_err(),
            "{bad:?} must be refused"
        );
    }
    let blob_oid = String::from_utf8(git(dir.path(), &["rev-parse", "HEAD:a.txt"]))
        .unwrap()
        .trim()
        .to_owned();
    assert!(reset::apply(&repo, ResetMode::Mixed, &blob_oid).is_err());

    // 3. Synthetic commit with a gitlink tree entry is refused by Mixed / Hard reset
    let objects = ObjectStore::open(repo.git_dir()).unwrap();
    let gitlink_tree = Tree {
        entries: vec![TreeEntry {
            mode: TreeMode::Gitlink,
            name: BString::from("sub"),
            oid: blob_oid.parse::<GitOid>().unwrap(),
        }],
    };
    let tree_oid = objects
        .write(&RawObject {
            kind: ObjectKind::Tree,
            data: gitlink_tree.serialize(),
        })
        .unwrap();
    let commit = Commit {
        tree: tree_oid,
        parents: Vec::new(),
        author: Signature::parse(b"t <t@e> 1 +0000").unwrap(),
        committer: Signature::parse(b"t <t@e> 1 +0000").unwrap(),
        extra_headers: Vec::new(),
        message: BString::from("with gitlink\n"),
    };
    let bad_commit_oid = objects
        .write(&RawObject {
            kind: ObjectKind::Commit,
            data: commit.serialize(),
        })
        .unwrap();
    let err = reset::apply(&repo, ResetMode::Hard, &bad_commit_oid.to_hex()).unwrap_err();
    assert!(err.to_string().contains("submodules"), "{err}");
}

fn setup_release_demo_repo() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    git(
        dir.path(),
        &["init", "--quiet", "-b", "main", "--object-format=sha1"],
    );

    fs::write(dir.path().join(".gitignore"), b"*.ignored\n").unwrap();
    fs::write(dir.path().join("staged_diff.txt"), b"commit v0\n").unwrap();
    fs::write(dir.path().join("to_delete.txt"), b"tracked to delete\n").unwrap();
    fs::write(dir.path().join("mode.sh"), b"echo mode\n").unwrap();
    git(
        dir.path(),
        &[
            "add",
            ".gitignore",
            "staged_diff.txt",
            "to_delete.txt",
            "mode.sh",
        ],
    );
    git(dir.path(), &["commit", "-q", "-m", "baseline"]);
    let c1 = String::from_utf8(git(dir.path(), &["rev-parse", "HEAD"]))
        .unwrap()
        .trim()
        .to_owned();

    fs::write(dir.path().join("staged_diff.txt"), b"staged v1\n").unwrap();
    git(dir.path(), &["add", "staged_diff.txt"]);
    fs::write(dir.path().join("staged_diff.txt"), b"worktree v2\n").unwrap();

    fs::remove_file(dir.path().join("to_delete.txt")).unwrap();

    let mode_path = dir.path().join("mode.sh");
    let mut perms = fs::metadata(&mode_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&mode_path, perms).unwrap();

    symlink("staged_diff.txt", dir.path().join("link.sym")).unwrap();
    let non_utf8_name = OsStr::from_bytes(b"non_utf8_\xff\xfe.dat");
    fs::write(dir.path().join(non_utf8_name), b"non-utf8 data\n").unwrap();
    git_bytes(
        dir.path(),
        &[OsStr::new("add"), OsStr::new("link.sym"), non_utf8_name],
    );
    fs::write(dir.path().join("untracked.txt"), b"keep untracked\n").unwrap();
    fs::write(dir.path().join("secret.ignored"), b"ignored bytes\n").unwrap();

    (dir, c1)
}

#[test]
fn full_release_demo_reset_hard_status_undo_redo() {
    let (dir, c1) = setup_release_demo_repo();
    let mode_path = dir.path().join("mode.sh");
    let non_utf8_name = OsStr::from_bytes(b"non_utf8_\xff\xfe.dat");

    let repo = LocalRepository::discover(dir.path()).unwrap();
    let adoption = repo.adopt(AdoptOptions::default()).unwrap();
    let raw_index_pre_reset = fs::read(dir.path().join(".git/index")).unwrap();

    // Reset hard back to HEAD (`c1`), matching Git
    let reset_res = reset::apply(&repo, ResetMode::Hard, &c1).unwrap();
    assert_eq!(reset_res.target_commit.to_hex(), c1);
    assert_eq!(
        fs::read(dir.path().join("staged_diff.txt")).unwrap(),
        b"commit v0\n"
    );
    assert!(dir.path().join("to_delete.txt").is_file());
    assert_eq!(
        fs::symlink_metadata(&mode_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert!(!dir.path().join("link.sym").exists());
    assert!(!dir.path().join(non_utf8_name).exists());
    assert_eq!(
        fs::read(dir.path().join("untracked.txt")).unwrap(),
        b"keep untracked\n"
    );
    assert_eq!(
        fs::read(dir.path().join("secret.ignored")).unwrap(),
        b"ignored bytes\n"
    );
    git(dir.path(), &["fsck", "--full"]);

    // Plain read-only `git status` refreshes stat-cache timestamps in `.git/index`
    git(dir.path(), &["status"]);
    let post_reset_stage = git(dir.path(), &["ls-files", "--stage", "-z"]);

    // `undo --dry-run` succeeds without writing or changing history
    let preview = undo::plan(&repo).unwrap();
    assert_eq!(preview.target_view, adoption.view_id);
    assert!(!preview.changes().is_empty());

    // `undo` restores exact raw pre-reset index and modeled worktree bytes while leaving ignored file alone
    let undo_res = undo::apply(&repo).unwrap();
    assert_eq!(undo_res.restored_view, adoption.view_id);
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        raw_index_pre_reset
    );
    assert_eq!(
        fs::read(dir.path().join("staged_diff.txt")).unwrap(),
        b"worktree v2\n"
    );
    assert!(!dir.path().join("to_delete.txt").exists());
    assert_eq!(
        fs::symlink_metadata(&mode_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(
        fs::read_link(dir.path().join("link.sym")).unwrap(),
        Path::new("staged_diff.txt")
    );
    assert_eq!(
        fs::read(dir.path().join(non_utf8_name)).unwrap(),
        b"non-utf8 data\n"
    );
    assert_eq!(
        fs::read(dir.path().join("untracked.txt")).unwrap(),
        b"keep untracked\n"
    );
    assert_eq!(
        fs::read(dir.path().join("secret.ignored")).unwrap(),
        b"ignored bytes\n"
    );

    // Undoing the undo redoes the post-reset state (`after_view` matches semantically, with the
    // stat-refreshed index snapshot preserved by the `external-git` import before undo)
    let redo_res = undo::apply(&repo).unwrap();
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let redo_view = store.read_view(redo_res.restored_view).unwrap();
    let reset_view = store.read_view(reset_res.after_view).unwrap();
    assert_eq!(redo_view.head, reset_view.head);
    assert_eq!(redo_view.refs, reset_view.refs);
    assert_eq!(redo_view.files, reset_view.files);
    assert_eq!(
        git(dir.path(), &["ls-files", "--stage", "-z"]),
        post_reset_stage
    );
    assert_eq!(
        fs::read(dir.path().join("staged_diff.txt")).unwrap(),
        b"commit v0\n"
    );
    assert_eq!(
        fs::read(dir.path().join("secret.ignored")).unwrap(),
        b"ignored bytes\n"
    );

    let ops = oplog_ops(&store);
    assert!(ops.iter().any(|(_, op)| {
        matches!(&op.kind, OperationKind::Git2(cmd) if cmd.as_slice() == format!("reset --hard {c1}").as_bytes())
    }));
    let undo_count = ops
        .iter()
        .filter(|(_, op)| matches!(&op.kind, OperationKind::Git2(cmd) if cmd.as_slice() == b"undo"))
        .count();
    assert_eq!(undo_count, 2);
}

#[test]
fn undo_refuses_divergence_and_ineligible_operations() {
    let dir = init_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let adopt = repo.adopt(AdoptOptions::default()).unwrap();

    // 1. Refuse `adopt`
    let err_adopt = undo::plan(&repo).unwrap_err();
    assert!(err_adopt.to_string().contains("adoption"), "{err_adopt}");

    // 2. Refuse `checkpoint` with explicit restore hint
    fs::write(dir.path().join("base.txt"), b"cp edit\n").unwrap();
    let cp = repo.checkpoint().unwrap();
    let err_cp = undo::plan(&repo).unwrap_err();
    assert!(
        err_cp
            .to_string()
            .contains(&format!("git2 restore --view {}", adopt.view_id)),
        "{err_cp}"
    );
    assert_ne!(cp.operation_id, adopt.operation_id);

    // 3. `git status` + `import_external_changes` (stat-cache-only) does NOT block `undo`
    reset::apply(&repo, ResetMode::Hard, "HEAD").unwrap();
    git(dir.path(), &["status"]);
    repo.import_external_changes().unwrap();
    assert!(undo::plan(&repo).is_ok());

    // 4. Staged entry/mode/OID/flag divergence or worktree edit MUST block `undo`
    fs::write(dir.path().join("base.txt"), b"external stage\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    let err_div = undo::plan(&repo).unwrap_err();
    assert!(err_div.to_string().contains("diverged"), "{err_div}");
    let err_ext = undo::apply(&repo).unwrap_err();
    assert!(err_ext.to_string().contains("external-git"), "{err_ext}");
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "--quiet", "-b", "main"]);
    fs::write(dir.path().join("base.txt"), b"base\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "initial"]);
    dir
}
