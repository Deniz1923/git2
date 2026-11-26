#![cfg(unix)]

use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2_core::recovery::{ContentId, Operation, RecoveryStore};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::restore;
use git2_core::store::ObjectStore;
use git2_core::transaction::{self, escape_path_bytes, format_change_line};

fn git_cmd(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "git2 test")
        .env("GIT_AUTHOR_EMAIL", "git2@example.invalid")
        .env("GIT_COMMITTER_NAME", "git2 test")
        .env("GIT_COMMITTER_EMAIL", "git2@example.invalid")
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
        .env("GIT_COMMITTER_NAME", "git2 test")
        .env("GIT_COMMITTER_EMAIL", "git2@example.invalid")
        .output()
        .unwrap()
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = git_cmd(path, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(
        dir.path(),
        &["init", "--quiet", "-b", "main", "--object-format=sha1"],
    );
    fs::write(dir.path().join("base.txt"), b"base\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "initial"]);
    dir
}

fn oplog_ids(store: &RecoveryStore) -> Vec<(ContentId, Operation)> {
    let mut out = Vec::new();
    let mut cur = Some(store.head().unwrap());
    while let Some(id) = cur {
        let op = store.read_operation(id).unwrap();
        cur = op.parent;
        out.push((id, op));
    }
    out
}

fn snapshot_worktree_state(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
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
                let target = fs::read_link(&path).unwrap();
                out.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    target.as_os_str().as_bytes().to_vec(),
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

#[test]
fn preview_does_not_move_heads_or_append_history() {
    let dir = init_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let base_cp = repo.adopt(AdoptOptions::default()).unwrap();

    // Mutate worktree including a filename containing a newline and invalid UTF-8 bytes
    let weird_name = b"dir/bad\nname_\xff.txt";
    fs::create_dir_all(dir.path().join("dir")).unwrap();
    let weird_path = dir.path().join(OsStr::from_bytes(weird_name));
    fs::write(&weird_path, b"weird bytes\n").unwrap();
    fs::write(dir.path().join("base.txt"), b"modified\n").unwrap();

    let next_cp = repo.checkpoint().unwrap();
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let head_before = store.head().unwrap();
    let view_before = store.current_view().unwrap();
    let ops_before = oplog_ids(&store).len();
    let disk_before = snapshot_worktree_state(dir.path());
    let index_before = fs::read(dir.path().join(".git/index")).unwrap();
    let git_head_before = fs::read(dir.path().join(".git/HEAD")).unwrap();

    let plan = restore::plan(&repo, base_cp.view_id).unwrap();
    assert_eq!(plan.before_view, next_cp.view_id);
    assert_eq!(plan.target_view, base_cp.view_id);
    assert!(!plan.changes().is_empty());

    let lines: Vec<String> = plan.changes().iter().map(format_change_line).collect();
    assert!(
        lines.iter().all(|line| !line.contains('\n')),
        "preview lines must be byte-safe single lines: {lines:#?}"
    );
    let escaped = escape_path_bytes(weird_name);
    assert!(escaped.contains("\\n") && escaped.contains("\\xff"));
    assert!(
        lines
            .iter()
            .any(|line| line.contains(&escaped) && line.starts_with("remove worktree ")),
        "expected escaped removal line in {lines:#?}"
    );

    assert_eq!(store.head().unwrap(), head_before);
    assert_eq!(store.current_view().unwrap(), view_before);
    assert_eq!(oplog_ids(&store).len(), ops_before);
    assert_eq!(snapshot_worktree_state(dir.path()), disk_before);
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        index_before
    );
    assert_eq!(
        fs::read(dir.path().join(".git/HEAD")).unwrap(),
        git_head_before
    );
}

#[test]
fn ignored_child_blocks_directory_replacement_without_writes() {
    let dir = init_repo();
    fs::write(dir.path().join(".gitignore"), b"*.log\n").unwrap();
    fs::write(dir.path().join("node"), b"i am a regular file\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "node"]);
    git(dir.path(), &["commit", "-q", "-m", "node as file"]);

    let repo = LocalRepository::discover(dir.path()).unwrap();
    let target_cp = repo.adopt(AdoptOptions::default()).unwrap();

    // Replace `node` with a directory holding both tracked and ignored files
    git(dir.path(), &["rm", "-q", "node"]);
    fs::create_dir(dir.path().join("node")).unwrap();
    fs::write(dir.path().join("node/tracked.txt"), b"tracked inside\n").unwrap();
    fs::write(
        dir.path().join("node/secret.log"),
        b"precious ignored data\n",
    )
    .unwrap();
    git(dir.path(), &["add", "node/tracked.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "node as dir"]);
    let before_cp = repo.checkpoint().unwrap();

    let err_plan = restore::plan(&repo, target_cp.view_id).unwrap_err();
    assert!(
        err_plan.to_string().contains("ignored entry") && err_plan.to_string().contains("blocks"),
        "unexpected plan error: {err_plan}"
    );

    let err_apply = restore::apply(&repo, target_cp.view_id).unwrap_err();
    assert!(err_apply.to_string().contains("ignored entry"));

    // Both ignored child and repository state must be completely untouched
    assert_eq!(
        fs::read(dir.path().join("node/secret.log")).unwrap(),
        b"precious ignored data\n"
    );
    assert_eq!(
        fs::read(dir.path().join("node/tracked.txt")).unwrap(),
        b"tracked inside\n"
    );
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    assert_eq!(store.head().unwrap(), before_cp.operation_id);
    assert!(!store.has_pending().unwrap());
}

#[test]
fn uses_both_ignore_profiles_when_gitignore_changes() {
    let dir = init_repo();
    fs::write(dir.path().join(".gitignore"), b"*.tmp\n").unwrap();
    fs::write(dir.path().join("note.txt"), b"visible note\n").unwrap();
    git(dir.path(), &["add", ".gitignore", "note.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "v1"]);

    let repo = LocalRepository::discover(dir.path()).unwrap();
    let v1_cp = repo.adopt(AdoptOptions::default()).unwrap();

    // v2 drops `*.tmp` rule and does NOT capture any `.tmp` files
    fs::write(dir.path().join(".gitignore"), b"").unwrap();
    git(dir.path(), &["add", ".gitignore"]);
    git(dir.path(), &["commit", "-q", "-m", "v2"]);
    let v2_empty_cp = repo.checkpoint().unwrap();

    // v3 adds an untracked non-ignored `target-only.tmp` while in v2's ignore profile
    fs::write(
        dir.path().join("target-only.tmp"),
        b"kept across ignore rule change\n",
    )
    .unwrap();
    repo.checkpoint().unwrap();

    // Case A: restoring to v1 where `target-only.tmp` becomes ignored must keep it without removal
    restore::apply(&repo, v1_cp.view_id).unwrap();
    assert_eq!(
        fs::read(dir.path().join("target-only.tmp")).unwrap(),
        b"kept across ignore rule change\n"
    );
    assert_eq!(fs::read(dir.path().join(".gitignore")).unwrap(), b"*.tmp\n");

    // Case B: restoring to v2 (which does NOT contain `target-only.tmp` and does NOT ignore it)
    // while `target-only.tmp` is currently ignored MUST refuse without writes
    let err = restore::apply(&repo, v2_empty_cp.view_id).unwrap_err();
    assert!(
        err.to_string().contains("non-ignored untracked"),
        "expected ambiguous reclassification error, got: {err}"
    );
    assert_eq!(fs::read(dir.path().join(".gitignore")).unwrap(), b"*.tmp\n");
    assert_eq!(
        fs::read(dir.path().join("target-only.tmp")).unwrap(),
        b"kept across ignore rule change\n"
    );
}

#[test]
fn reclassifies_ignored_tracked_path_only_without_worktree_write() {
    let dir = init_repo();
    fs::write(dir.path().join(".gitignore"), b"secret.cfg\n").unwrap();
    fs::write(dir.path().join("secret.cfg"), b"same-content\n").unwrap();
    git(dir.path(), &["add", ".gitignore"]);
    git(dir.path(), &["add", "-f", "secret.cfg"]);
    git(
        dir.path(),
        &["commit", "-q", "-m", "force tracked ignored file"],
    );

    let repo = LocalRepository::discover(dir.path()).unwrap();
    let tracked_view = repo.adopt(AdoptOptions::default()).unwrap();

    // Untrack `secret.cfg` via rm --cached; file stays on disk as ignored
    git(dir.path(), &["rm", "--cached", "-q", "secret.cfg"]);
    git(dir.path(), &["commit", "-q", "-m", "untrack secret.cfg"]);

    // Case 1: identical on-disk ignored file matches target tracked entry -> allowed with no worktree write
    let plan_same = restore::plan(&repo, tracked_view.view_id).unwrap();
    assert!(
        !plan_same
            .changes()
            .iter()
            .any(|c| c.path.as_slice() == b"secret.cfg"),
        "identical reclassified file should not rewrite worktree: {:?}",
        plan_same.changes()
    );
    let op1 = restore::apply(&repo, tracked_view.view_id).unwrap();
    assert_eq!(
        fs::read(dir.path().join("secret.cfg")).unwrap(),
        b"same-content\n"
    );

    // Case 2: move to untracked commit, edit ignored file, try restoring back -> MUST refuse without overwriting
    git(dir.path(), &["rm", "--cached", "-q", "secret.cfg"]);
    git(dir.path(), &["commit", "-q", "-m", "untrack again"]);
    let untracked_cp = repo.checkpoint().unwrap();
    fs::write(dir.path().join("secret.cfg"), b"user modified ignored\n").unwrap();

    let err = restore::apply(&repo, tracked_view.view_id).unwrap_err();
    assert!(
        err.to_string().contains("conflicts with target path"),
        "expected refusal on differing ignored file, got: {err}"
    );
    assert_eq!(
        fs::read(dir.path().join("secret.cfg")).unwrap(),
        b"user modified ignored\n"
    );

    // Case 3: force-tracked -> ignored (target does not track it; current index does) keeps disk bytes
    fs::write(dir.path().join("secret.cfg"), b"from tracked state\n").unwrap();
    git(dir.path(), &["add", "-f", "secret.cfg"]);
    git(dir.path(), &["commit", "-q", "-m", "track again"]);
    let plan_to_untracked = restore::plan(&repo, untracked_cp.view_id).unwrap();
    assert!(
        !plan_to_untracked
            .changes()
            .iter()
            .any(|c| c.path.as_slice() == b"secret.cfg"),
        "reclassified tracked->ignored file should not remove disk copy: {:?}",
        plan_to_untracked.changes()
    );
    let op3 = restore::apply(&repo, untracked_cp.view_id).unwrap();
    assert_ne!(op1, op3);
    assert_eq!(
        fs::read(dir.path().join("secret.cfg")).unwrap(),
        b"from tracked state\n"
    );
}

#[test]
fn corrupt_or_missing_target_content_refuses_before_writes() {
    let dir = init_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let v1 = repo.adopt(AdoptOptions::default()).unwrap();

    fs::write(dir.path().join("base.txt"), b"changed\n").unwrap();
    git(dir.path(), &["add", "base.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "v2"]);
    let v2 = repo.checkpoint().unwrap();

    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let v1_view = store.read_view(v1.view_id).unwrap();
    let base_record = v1_view
        .files
        .iter()
        .find(|f| f.path.as_slice() == b"base.txt")
        .unwrap();

    // Corrupt a CAS payload needed ONLY by v1
    let hex = base_record.content.to_hex();
    let cas_path = dir
        .path()
        .join(".git/git2/cas")
        .join(&hex[..2])
        .join(&hex[2..]);
    let mut perms = fs::metadata(&cas_path).unwrap().permissions();
    perms.set_mode(0o600);
    fs::set_permissions(&cas_path, perms).unwrap();
    fs::write(&cas_path, b"not valid zlib").unwrap();

    assert!(restore::plan(&repo, v1.view_id).is_err());
    assert!(restore::apply(&repo, v1.view_id).is_err());
    assert_eq!(fs::read(dir.path().join("base.txt")).unwrap(), b"changed\n");
    assert_eq!(store.head().unwrap(), v2.operation_id);
    assert!(!store.has_pending().unwrap());
}

fn setup_rich_fixture(root: &Path) {
    fs::write(root.join(".gitignore"), b"*.ignored\n").unwrap();
    fs::write(root.join("keep.ignored"), b"untouched ignored bytes\n").unwrap();
    fs::write(root.join("reg.bin"), b"regular\x00bytes\n").unwrap();

    let exec_path = root.join("script.sh");
    fs::write(&exec_path, b"#!/bin/sh\necho hi\n").unwrap();
    let mut perms = fs::metadata(&exec_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&exec_path, perms).unwrap();

    symlink("reg.bin", root.join("link.sym")).unwrap();

    let non_utf8_name = OsStr::from_bytes(b"raw_\xfe\x80_path");
    fs::write(root.join(non_utf8_name), b"non-utf8 content\n").unwrap();

    git(
        root,
        &["add", ".gitignore", "reg.bin", "script.sh", "link.sym"],
    );
    git_bytes(root, &[OsStr::new("add"), non_utf8_name]);
    git(root, &["commit", "-q", "-m", "rich state"]);
}

fn orphan_and_prune(root: &Path) {
    git(root, &["checkout", "--orphan", "wipe"]);
    git(root, &["rm", "-rfq", "."]);
    fs::write(root.join("clean.txt"), b"only clean\n").unwrap();
    git(root, &["add", "clean.txt"]);
    git(root, &["commit", "-q", "-m", "orphan wipe"]);
    git(root, &["branch", "-D", "main"]);
    git(root, &["branch", "-D", "shadow"]);
    let _ = fs::remove_file(root.join(".git/packed-refs"));
    git(root, &["reflog", "expire", "--expire=now", "--all"]);
    git(root, &["gc", "--prune=now", "--quiet"]);
}

#[test]
fn restore_round_trips_supported_bytes_after_prune() {
    let dir = init_repo();
    setup_rich_fixture(dir.path());
    let exec_path = dir.path().join("script.sh");
    let non_utf8_name = OsStr::from_bytes(b"raw_\xfe\x80_path");

    // Create a branch ref packed and ALSO shadowed by a loose ref
    git(dir.path(), &["checkout", "-q", "-b", "shadow"]);
    git(dir.path(), &["pack-refs", "--all"]);
    let packed_oid = git(dir.path(), &["rev-parse", "main"]).trim().to_owned();
    // Move loose `shadow` forward after packing so packed and loose differ
    fs::write(dir.path().join("extra.txt"), b"advance shadow\n").unwrap();
    git(dir.path(), &["add", "extra.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "shadow target"]);
    let target_head_oid = git(dir.path(), &["rev-parse", "HEAD"]).trim().to_owned();

    // Add staged-only blob (never committed!) and untracked file AFTER branch setup
    fs::write(dir.path().join("staged_only.txt"), b"staged content v1\n").unwrap();
    git(dir.path(), &["add", "staged_only.txt"]);
    let staged_oid_hex = git(dir.path(), &["rev-parse", ":staged_only.txt"])
        .trim()
        .to_owned();
    // Overwrite worktree copy while leaving index at `staged content v1\n`
    fs::write(dir.path().join("staged_only.txt"), b"worktree edited v2\n").unwrap();
    fs::write(dir.path().join("untracked.txt"), b"untracked bytes\n").unwrap();

    let repo = LocalRepository::discover(dir.path()).unwrap();
    let rich_cp = repo.adopt(AdoptOptions::default()).unwrap();
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let rich_view = store.read_view(rich_cp.view_id).unwrap();
    let rich_index_bytes = fs::read(dir.path().join(".git/index")).unwrap();
    let rich_head_bytes = fs::read(dir.path().join(".git/HEAD")).unwrap();
    let rich_packed_refs = fs::read(dir.path().join(".git/packed-refs")).unwrap();

    // Move to a totally unrelated orphan commit and prune EVERYTHING reachable only from rich_view
    orphan_and_prune(dir.path());

    // Verify staged-only blob AND target commit are GONE from ordinary `.git/objects/`
    let objects = ObjectStore::open(repo.git_dir()).unwrap();
    let staged_oid = staged_oid_hex.parse().unwrap();
    let target_commit_oid = target_head_oid.parse().unwrap();
    let packed_commit_oid = packed_oid.parse().unwrap();
    assert!(
        objects.read(staged_oid).is_err(),
        "staged-only blob must be pruned"
    );
    assert!(
        objects.read(target_commit_oid).is_err(),
        "shadow commit must be pruned"
    );
    assert!(
        objects.read(packed_commit_oid).is_err(),
        "packed ref target commit must be pruned"
    );

    // Restore back to `rich_cp.view_id`
    let restore_op = restore::apply(&repo, rich_cp.view_id).unwrap();
    let applied_op = store.read_operation(restore_op).unwrap();
    assert_eq!(applied_op.after, rich_cp.view_id);

    // Every byte, kind, staged blob, packed/loose ref, non-UTF8 path, symlink, and ignored file matches
    assert_eq!(
        fs::read(dir.path().join(".git/HEAD")).unwrap(),
        rich_head_bytes
    );
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        rich_index_bytes
    );
    assert_eq!(
        fs::read(dir.path().join(".git/packed-refs")).unwrap(),
        rich_packed_refs
    );
    assert_eq!(
        fs::read(dir.path().join("reg.bin")).unwrap(),
        b"regular\x00bytes\n"
    );
    assert_eq!(
        fs::symlink_metadata(&exec_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
    assert_eq!(
        fs::read_link(dir.path().join("link.sym")).unwrap(),
        Path::new("reg.bin")
    );
    assert_eq!(
        fs::read(dir.path().join(non_utf8_name)).unwrap(),
        b"non-utf8 content\n"
    );
    assert_eq!(
        fs::read(dir.path().join("staged_only.txt")).unwrap(),
        b"worktree edited v2\n"
    );
    assert_eq!(
        fs::read(dir.path().join("untracked.txt")).unwrap(),
        b"untracked bytes\n"
    );
    assert_eq!(
        fs::read(dir.path().join("keep.ignored")).unwrap(),
        b"untouched ignored bytes\n"
    );
    assert!(!dir.path().join("clean.txt").exists());
    assert_eq!(
        objects.read(staged_oid).unwrap().data,
        b"staged content v1\n"
    );
    assert_eq!(store.current_view().unwrap(), rich_cp.view_id);
    assert_eq!(rich_view.head.as_slice(), rich_head_bytes.as_slice());

    git(dir.path(), &["fsck", "--full"]);
}

#[test]
fn restore_swaps_file_and_directory_and_df_refs() {
    let dir = init_repo();
    fs::write(dir.path().join("swap"), b"swap as file\n").unwrap();
    git(dir.path(), &["add", "swap"]);
    git(dir.path(), &["commit", "-q", "-m", "file swap"]);
    git(dir.path(), &["branch", "topic"]);

    let repo = LocalRepository::discover(dir.path()).unwrap();
    let v_file = repo.adopt(AdoptOptions::default()).unwrap();

    git(dir.path(), &["rm", "-q", "swap"]);
    fs::create_dir(dir.path().join("swap")).unwrap();
    fs::write(dir.path().join("swap/nested.txt"), b"nested child\n").unwrap();
    git(dir.path(), &["add", "swap/nested.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "dir swap"]);
    git(dir.path(), &["branch", "-D", "topic"]);
    git(dir.path(), &["branch", "topic/sub"]);
    let v_dir = repo.checkpoint().unwrap();

    // Dir -> File + `topic/sub` -> `topic`
    restore::apply(&repo, v_file.view_id).unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("swap"))
            .unwrap()
            .is_file()
    );
    assert_eq!(
        fs::read(dir.path().join("swap")).unwrap(),
        b"swap as file\n"
    );
    assert!(dir.path().join(".git/refs/heads/topic").is_file());
    assert!(!dir.path().join(".git/refs/heads/topic/sub").exists());
    git(dir.path(), &["fsck", "--full"]);

    // File -> Dir + `topic` -> `topic/sub`
    restore::apply(&repo, v_dir.view_id).unwrap();
    assert!(
        fs::symlink_metadata(dir.path().join("swap"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(
        fs::read(dir.path().join("swap/nested.txt")).unwrap(),
        b"nested child\n"
    );
    assert!(dir.path().join(".git/refs/heads/topic/sub").is_file());
    git(dir.path(), &["fsck", "--full"]);

    // Verify recovery handles no pending left behind
    let rec = transaction::RecoveryRepository::open(dir.path()).unwrap();
    assert!(transaction::pending(&rec).unwrap().is_none());
}
