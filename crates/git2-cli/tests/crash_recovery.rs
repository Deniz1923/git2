#![cfg(all(unix, debug_assertions))]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git2(directory: &Path, arguments: &[&str], failpoint: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_git2"));
    cmd.args(arguments).current_dir(directory);
    if let Some(fp) = failpoint {
        cmd.env("GIT2_TEST_FAILPOINT", fp);
    }
    cmd.output().expect("run git2")
}

fn git(directory: &Path, arguments: &[&str]) -> String {
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
        .expect("run git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn parse_view_id(output: &Output) -> String {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let (_, id) = stdout
        .trim_end()
        .rsplit_once(" as view ")
        .unwrap_or_else(|| panic!("expected 'as view <id>', got: {stdout:?}"));
    assert_eq!(id.len(), 64, "expected 64-hex view id, got {id:?}");
    id.to_owned()
}

fn init_swap_repo() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    git(
        dir.path(),
        &["init", "--quiet", "-b", "main", "--object-format=sha1"],
    );
    fs::write(dir.path().join("item"), b"item as file\n").unwrap();
    git(dir.path(), &["add", "item"]);
    git(dir.path(), &["commit", "-q", "-m", "v1 file"]);
    let adopt_out = git2(dir.path(), &["adopt"], None);
    assert!(adopt_out.status.success());
    let file_view = parse_view_id(&adopt_out);

    git(dir.path(), &["rm", "-q", "item"]);
    fs::create_dir(dir.path().join("item")).unwrap();
    fs::write(dir.path().join("item/child.txt"), b"child content\n").unwrap();
    git(dir.path(), &["add", "item/child.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "v2 dir"]);
    let cp_out = git2(dir.path(), &["checkpoint"], None);
    assert!(cp_out.status.success());
    let dir_view = parse_view_id(&cp_out);
    (dir, file_view, dir_view)
}

#[test]
fn recovers_from_partial_directory_swap() {
    let (dir, file_view, _) = init_swap_repo();

    // Kill right after removing `item/child.txt` (`before_action:1` in dir->file swap:
    // action 0 = RemoveWorktreeEntry(item/child.txt), action 1 = RemoveEmptyDir(item))
    let crashed = git2(
        dir.path(),
        &["restore", "--view", &file_view],
        Some("abort:before_action:1"),
    );
    assert!(!crashed.status.success(), "child should abort at failpoint");
    assert!(dir.path().join(".git/git2/pending/active").is_file());
    assert!(!dir.path().join("item/child.txt").exists());
    assert!(dir.path().join("item").is_dir());

    let rec = git2(dir.path(), &["recover"], None);
    assert!(
        rec.status.success(),
        "recover failed: {}",
        String::from_utf8_lossy(&rec.stderr)
    );
    let stdout = String::from_utf8(rec.stdout).unwrap();
    assert!(
        stdout.contains("recovered operation"),
        "unexpected stdout: {stdout}"
    );
    assert!(!dir.path().join(".git/git2/pending/active").exists());
    assert!(
        fs::symlink_metadata(dir.path().join("item"))
            .unwrap()
            .is_file()
    );
    assert_eq!(
        fs::read(dir.path().join("item")).unwrap(),
        b"item as file\n"
    );
    git(dir.path(), &["fsck", "--full"]);
}

#[test]
fn recovery_after_second_kill_has_one_selected_operation() {
    let (dir, file_view, _) = init_swap_repo();

    // Kill AFTER recovery HEAD moved to restore operation, BEFORE pending cleanup
    let crashed1 = git2(
        dir.path(),
        &["restore", "--view", &file_view],
        Some("abort:after_head_selection"),
    );
    assert!(!crashed1.status.success());
    assert!(dir.path().join(".git/git2/pending/active").is_file());

    // Simulate a second kill during `recover` right after seeing HEAD already moved? Wait: when
    // `after_head_selection` already moved HEAD, `recover` takes the early `current_head == expected_op_id`
    // cleanup branch. Also test killing during `recover_after_head_selection` from `before_action:1`!
    let crashed2_repo = init_swap_repo();
    let (dir2, file_view2, _) = crashed2_repo;
    let first_kill = git2(
        dir2.path(),
        &["restore", "--view", &file_view2],
        Some("abort:before_action:1"),
    );
    assert!(!first_kill.status.success());
    let second_kill = git2(
        dir2.path(),
        &["recover"],
        Some("abort:recover_after_head_selection"),
    );
    assert!(!second_kill.status.success());
    let final_rec = git2(dir2.path(), &["recover"], None);
    assert!(
        final_rec.status.success(),
        "final recover failed: {}",
        String::from_utf8_lossy(&final_rec.stderr)
    );
    assert!(!dir2.path().join(".git/git2/pending/active").exists());

    let rec1 = git2(dir.path(), &["recover"], None);
    assert!(rec1.status.success());
    assert!(!dir.path().join(".git/git2/pending/active").exists());

    for repo_dir in [dir.path(), dir2.path()] {
        let oplog = git2(repo_dir, &["oplog"], None);
        assert!(oplog.status.success());
        let log_text = String::from_utf8(oplog.stdout).unwrap();
        let restore_count = log_text
            .lines()
            .filter(|line| line.contains("git2:restore --view "))
            .count();
        assert_eq!(
            restore_count, 1,
            "expected 1 restore op in oplog:\n{log_text}"
        );
    }
}

#[test]
fn after_view_io_error_can_roll_back() {
    let (dir, file_view, dir_view) = init_swap_repo();

    let io_fail = git2(
        dir.path(),
        &["restore", "--view", &file_view],
        Some("io:before_finalize"),
    );
    assert!(
        !io_fail.status.success(),
        "injected I/O failure must abort apply"
    );
    assert!(dir.path().join(".git/git2/pending/active").is_file());

    let rb = git2(dir.path(), &["recover", "--rollback"], None);
    assert!(
        rb.status.success(),
        "recover --rollback failed: {}",
        String::from_utf8_lossy(&rb.stderr)
    );
    let rb_out = String::from_utf8(rb.stdout).unwrap();
    assert!(
        rb_out.contains(&dir_view),
        "expected dir_view {dir_view}: {rb_out}"
    );
    assert!(!dir.path().join(".git/git2/pending/active").exists());
    assert!(
        fs::symlink_metadata(dir.path().join("item"))
            .unwrap()
            .is_dir()
    );
    assert_eq!(
        fs::read(dir.path().join("item/child.txt")).unwrap(),
        b"child content\n"
    );
    git(dir.path(), &["fsck", "--full"]);
}

#[test]
fn recover_status_works_with_partial_layout() {
    let (dir, file_view, _) = init_swap_repo();

    let crashed = git2(
        dir.path(),
        &["restore", "--view", &file_view],
        Some("abort:before_action:1"),
    );
    assert!(!crashed.status.success());

    let pending_before = fs::read(dir.path().join(".git/git2/pending/active")).unwrap();
    let head_before = fs::read(dir.path().join(".git/git2/HEAD")).unwrap();

    let status = git2(dir.path(), &["recover", "--status"], None);
    assert!(
        status.status.success(),
        "recover --status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status_out = String::from_utf8(status.stdout).unwrap();
    assert!(status_out.contains("pending transaction"));
    assert!(status_out.contains("phase: applying"));
    assert!(status_out.contains(&file_view));

    // Pending `oplog` remains readable without modifying sidecar HEAD or pending state
    let oplog = git2(dir.path(), &["oplog"], None);
    assert!(oplog.status.success());
    let oplog_out = String::from_utf8(oplog.stdout).unwrap();
    assert!(oplog_out.contains("adopt"));
    assert!(oplog_out.contains("git2:checkpoint"));
    assert_eq!(
        fs::read(dir.path().join(".git/git2/pending/active")).unwrap(),
        pending_before
    );
    assert_eq!(
        fs::read(dir.path().join(".git/git2/HEAD")).unwrap(),
        head_before
    );
}

#[test]
fn foreign_lock_refusal_leaves_lock_intact() {
    let (dir, file_view, _) = init_swap_repo();
    let sentinel = b"foreign-index-lock";
    let lock_path = dir.path().join(".git/index.lock");
    fs::write(&lock_path, sentinel).unwrap();

    let blocked = git2(dir.path(), &["restore", "--view", &file_view], None);
    assert!(!blocked.status.success());
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert!(
        stderr.contains("unknown Git lock"),
        "expected unknown Git lock error, got: {stderr}"
    );
    assert_eq!(fs::read(&lock_path).unwrap(), sentinel);
    assert!(dir.path().join("item/child.txt").is_file());
}

#[test]
fn dry_run_reports_pending_without_modifying_state() {
    let (dir, file_view, _) = init_swap_repo();

    let crashed = git2(
        dir.path(),
        &["restore", "--view", &file_view],
        Some("abort:after_prepare"),
    );
    assert!(!crashed.status.success());

    let head_before = fs::read(dir.path().join(".git/git2/HEAD")).unwrap();
    let dry = git2(
        dir.path(),
        &["restore", "--view", &file_view, "--dry-run"],
        None,
    );
    assert!(!dry.status.success());
    let stderr = String::from_utf8_lossy(&dry.stderr);
    assert!(
        stderr.contains("pending transaction"),
        "expected pending transaction notice, got: {stderr}"
    );
    assert!(dir.path().join(".git/git2/pending/active").is_file());
    assert_eq!(
        fs::read(dir.path().join(".git/git2/HEAD")).unwrap(),
        head_before
    );
}

#[test]
fn status_and_checkpoint_report_pending_transaction_cleanly() {
    let (dir, file_view, _) = init_swap_repo();

    let crashed = git2(
        dir.path(),
        &["restore", "--view", &file_view],
        Some("abort:after_prepare"),
    );
    assert!(!crashed.status.success());

    // Even if `.git/index` disappeared mid-crash, `status` and `checkpoint` must direct
    // the user to `git2 recover` first rather than running repository discovery blindly.
    fs::remove_file(dir.path().join(".git/index")).unwrap();

    for cmd in [["status"].as_slice(), ["checkpoint"].as_slice()] {
        let out = git2(dir.path(), cmd, None);
        assert!(!out.status.success());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("pending transaction") && stderr.contains("git2 recover"),
            "{cmd:?} must report pending transaction: {stderr}"
        );
    }
}
