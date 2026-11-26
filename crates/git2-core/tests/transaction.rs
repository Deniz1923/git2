#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

use bstr::BString;
use git2_core::Error;
use git2_core::object::{GitOid, ObjectKind, parse_canonical_object};
use git2_core::recovery::{ContentId, FileKind, Operation, OperationKind, RecoveryStore, View};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::store::ObjectStore;
use git2_core::transaction::{
    Action, EntryState, Failpoint, MutationPlan, PendingPhase, RecoveryOutcome, RecoveryRepository,
    pending, recover, recover_with_hook, rollback, run, run_with_hook,
};

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

fn failpoint_tag(fp: Failpoint) -> String {
    match fp {
        Failpoint::BeforePendingWrite => "before-pending-write".to_owned(),
        Failpoint::AfterPrepare => "after-prepare".to_owned(),
        Failpoint::BeforeLockPublish(i) => format!("before-lock-publish:{i}"),
        Failpoint::AfterLockPublish(i) => format!("after-lock-publish:{i}"),
        Failpoint::BeforeAction(i) => format!("before-action:{i}"),
        Failpoint::AfterAction(i) => format!("after-action:{i}"),
        Failpoint::BeforeFinalize => "before-finalize".to_owned(),
        Failpoint::AfterOperationWrite => "after-operation-write".to_owned(),
        Failpoint::AfterHeadSelection => "after-head-selection".to_owned(),
        Failpoint::BeforePendingCleanup => "before-pending-cleanup".to_owned(),
        Failpoint::RecoverAfterOperationWrite => "recover-after-operation-write".to_owned(),
        Failpoint::RecoverAfterHeadSelection => "recover-after-head-selection".to_owned(),
    }
}

fn parse_failpoint(tag: &str) -> Failpoint {
    if let Some((kind, idx)) = tag.split_once(':') {
        let i: usize = idx.parse().unwrap();
        match kind {
            "before-lock-publish" => Failpoint::BeforeLockPublish(i),
            "after-lock-publish" => Failpoint::AfterLockPublish(i),
            "before-action" => Failpoint::BeforeAction(i),
            "after-action" => Failpoint::AfterAction(i),
            _ => panic!("unknown failpoint {tag}"),
        }
    } else {
        match tag {
            "before-pending-write" => Failpoint::BeforePendingWrite,
            "after-prepare" => Failpoint::AfterPrepare,
            "before-finalize" => Failpoint::BeforeFinalize,
            "after-operation-write" => Failpoint::AfterOperationWrite,
            "after-head-selection" => Failpoint::AfterHeadSelection,
            "before-pending-cleanup" => Failpoint::BeforePendingCleanup,
            "recover-after-operation-write" => Failpoint::RecoverAfterOperationWrite,
            "recover-after-head-selection" => Failpoint::RecoverAfterHeadSelection,
            _ => panic!("unknown failpoint {tag}"),
        }
    }
}

struct MultiFixture {
    plan: MutationPlan,
    pruned_oid: GitOid,
    new_index_bytes: Vec<u8>,
    new_commit_hex: String,
    before_index_bytes: Vec<u8>,
    before_head_bytes: Vec<u8>,
}

fn setup_multi_fixture(path: &Path) -> MultiFixture {
    // Prepare pre-state: `base.txt`, `remove.txt`, empty dir `gone_dir/`, plus an ignored file.
    fs::write(path.join(".gitignore"), b"ignored.txt\n").unwrap();
    fs::write(path.join("ignored.txt"), b"must stay untouched\n").unwrap();
    fs::write(path.join("remove.txt"), b"to remove\n").unwrap();
    fs::create_dir(path.join("gone_dir")).unwrap();
    git(path, &["add", ".gitignore", "remove.txt"]);
    git(path, &["commit", "-q", "-m", "pre-commit"]);

    // Create a second commit + staged state on a branch `target`, save its view & objects,
    // then reset back and prune so `RehydrateObject` is genuinely exercised.
    let pre_commit_hex = git(path, &["rev-parse", "HEAD"]).trim().to_owned();
    let repo = LocalRepository::discover(path).unwrap();
    let adoption = repo.adopt(AdoptOptions::default()).unwrap();
    let store = RecoveryStore::open(repo.git_dir()).unwrap();

    // Build target state: remove `remove.txt`, remove `gone_dir`, create `nested/new.sh` (exec),
    // stage a unique blob not present in `main`, and commit on `refs/heads/feature`.
    fs::remove_file(path.join("remove.txt")).unwrap();
    fs::remove_dir(path.join("gone_dir")).unwrap();
    fs::create_dir(path.join("nested")).unwrap();
    fs::write(path.join("nested/new.sh"), b"#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(
        path.join("nested/new.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    git(path, &["add", "-A", "remove.txt", "nested/new.sh"]);
    let staged_oid: GitOid = git(path, &["rev-parse", ":nested/new.sh"])
        .trim()
        .parse()
        .unwrap();
    git(path, &["checkout", "-q", "-b", "feature"]);
    git(path, &["commit", "-q", "-m", "feature commit"]);
    let new_commit_hex = git(path, &["rev-parse", "HEAD"]).trim().to_owned();
    let new_index_bytes = fs::read(path.join(".git/index")).unwrap();
    let target_cp = repo.checkpoint().unwrap();
    let target_view = store.read_view(target_cp.view_id).unwrap();

    // Switch back to `main` at pre-commit and prune the unique feature objects from `.git/objects/`.
    git(path, &["checkout", "-q", "main"]);
    git(path, &["branch", "-D", "feature"]);
    git(path, &["reset", "--hard", "-q", &pre_commit_hex]);
    fs::create_dir_all(path.join("gone_dir")).unwrap();
    git(path, &["reflog", "expire", "--expire=now", "--all"]);
    git(path, &["prune", "--expire=now"]);
    assert!(
        !git_cmd(path, &["cat-file", "-e", &staged_oid.to_hex()])
            .status
            .success(),
        "target staged blob must be pruned from Git before transaction"
    );

    let before_cp = repo.checkpoint().unwrap();
    let before_view = store.read_view(before_cp.view_id).unwrap();
    let before_index_bytes = fs::read(path.join(".git/index")).unwrap();
    let before_head_bytes = fs::read(path.join(".git/HEAD")).unwrap();
    let actions = build_multi_actions(
        &before_view,
        &target_view,
        staged_oid,
        &new_commit_hex,
        &before_head_bytes,
    );
    let _ = adoption;
    MultiFixture {
        plan: MutationPlan {
            expected_head: before_cp.operation_id,
            before_view: before_cp.view_id,
            target_view: target_cp.view_id,
            command: BString::from("synthetic-multi"),
            actions,
        },
        pruned_oid: staged_oid,
        new_index_bytes,
        new_commit_hex,
        before_index_bytes,
        before_head_bytes,
    }
}

fn build_multi_actions(
    before_view: &View,
    target_view: &View,
    staged_oid: GitOid,
    new_commit_hex: &str,
    before_head_bytes: &[u8],
) -> Vec<Action> {
    let new_index_id = target_view.index.unwrap();
    let before_index_id = before_view.index.unwrap();
    let remove_entry = before_view
        .files
        .iter()
        .find(|f| f.path.as_slice() == b"remove.txt")
        .unwrap();
    let new_entry = target_view
        .files
        .iter()
        .find(|f| f.path.as_slice() == b"nested/new.sh")
        .unwrap();

    let mut actions = Vec::new();
    let staged_ret = target_view
        .retained_objects
        .iter()
        .find(|r| r.oid == staged_oid)
        .unwrap();
    actions.push(Action::RehydrateObject {
        oid: staged_ret.oid,
        content: staged_ret.content,
    });
    for r in &target_view.retained_objects {
        if r.oid != staged_oid {
            actions.push(Action::RehydrateObject {
                oid: r.oid,
                content: r.content,
            });
        }
    }
    actions.push(Action::RemoveWorktreeEntry {
        path: BString::from("remove.txt"),
        expected_before: EntryState {
            kind: remove_entry.kind,
            content: remove_entry.content,
        },
    });
    actions.push(Action::RemoveEmptyDir {
        path: BString::from("gone_dir"),
    });
    actions.push(Action::EnsureDir {
        path: BString::from("nested"),
    });
    actions.push(Action::WriteWorktreeEntry {
        path: BString::from("nested/new.sh"),
        expected_before: None,
        target: EntryState {
            kind: new_entry.kind,
            content: new_entry.content,
        },
    });
    actions.push(Action::WriteIndex {
        expected_before: Some(before_index_id),
        target: new_index_id,
    });
    actions.push(Action::WriteRef {
        name: BString::from("refs/heads/feature"),
        expected_before: None,
        target: BString::from(format!("{new_commit_hex}\n")),
    });
    actions.push(Action::WriteHead {
        expected_before: BString::from(before_head_bytes),
        target: BString::from("ref: refs/heads/feature\n"),
    });
    actions
}

fn spawn_child_abort(
    repo_path: &Path,
    mode: &str,
    failpoint: Failpoint,
    plan_file: &Path,
) -> Output {
    let exe = std::env::current_exe().unwrap();
    Command::new(exe)
        .args(["--exact", "transaction_child_runner", "--nocapture"])
        .env("GIT2_TX_CHILD_MODE", mode)
        .env("GIT2_TX_CHILD_REPO", repo_path)
        .env("GIT2_TX_CHILD_FAILPOINT", failpoint_tag(failpoint))
        .env("GIT2_TX_CHILD_PLAN", plan_file)
        .output()
        .unwrap()
}

fn write_plan_fixture(file: &Path, plan: &MutationPlan) {
    // Simple test-only line serialization for passing `MutationPlan` to child process.
    let mut out = Vec::new();
    out.extend_from_slice(plan.expected_head.to_hex().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(plan.before_view.to_hex().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(plan.target_view.to_hex().as_bytes());
    out.push(b'\n');
    out.extend_from_slice(plan.command.as_ref());
    out.push(b'\n');
    out.extend_from_slice(plan.actions.len().to_string().as_bytes());
    out.push(b'\n');
    for action in &plan.actions {
        let line = serialize_action(action);
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    fs::write(file, out).unwrap();
}

fn serialize_action(action: &Action) -> String {
    match action {
        Action::RehydrateObject { oid, content } => {
            format!("rehydrate|{}|{}", oid.to_hex(), content.to_hex())
        }
        Action::WriteWorktreeEntry {
            path,
            expected_before,
            target,
        } => {
            let before = expected_before.map_or_else(
                || "none".to_owned(),
                |b| format!("{}:{}", kind_str(b.kind), b.content.to_hex()),
            );
            format!(
                "write-wt|{}|{}|{}:{}",
                hex_encode(path.as_ref()),
                before,
                kind_str(target.kind),
                target.content.to_hex()
            )
        }
        Action::RemoveWorktreeEntry {
            path,
            expected_before,
        } => {
            format!(
                "rm-wt|{}|{}:{}",
                hex_encode(path.as_ref()),
                kind_str(expected_before.kind),
                expected_before.content.to_hex()
            )
        }
        Action::EnsureDir { path } => format!("ensure-dir|{}", hex_encode(path.as_ref())),
        Action::RemoveEmptyDir { path } => format!("rm-dir|{}", hex_encode(path.as_ref())),
        Action::WriteIndex {
            expected_before,
            target,
        } => {
            let before = expected_before.map_or_else(|| "none".to_owned(), ContentId::to_hex);
            format!("write-idx|{before}|{}", target.to_hex())
        }
        Action::RemoveIndex { expected_before } => format!("rm-idx|{}", expected_before.to_hex()),
        Action::WriteRef {
            name,
            expected_before,
            target,
        } => {
            let before = expected_before
                .as_ref()
                .map_or_else(|| "none".to_owned(), |b| hex_encode(b.as_ref()));
            format!(
                "write-ref|{}|{}|{}",
                hex_encode(name.as_ref()),
                before,
                hex_encode(target.as_ref())
            )
        }
        Action::RemoveRef {
            name,
            expected_before,
        } => format!(
            "rm-ref|{}|{}",
            hex_encode(name.as_ref()),
            hex_encode(expected_before.as_ref())
        ),
        Action::WritePackedRefs {
            expected_before,
            target,
        } => {
            let before = expected_before
                .as_ref()
                .map_or_else(|| "none".to_owned(), |b| hex_encode(b.as_ref()));
            let after = target
                .as_ref()
                .map_or_else(|| "none".to_owned(), |b| hex_encode(b.as_ref()));
            format!("write-packed|{before}|{after}")
        }
        Action::WriteHead {
            expected_before,
            target,
        } => format!(
            "write-head|{}|{}",
            hex_encode(expected_before.as_ref()),
            hex_encode(target.as_ref())
        ),
    }
}

fn read_plan_fixture(file: &Path) -> MutationPlan {
    let text = fs::read_to_string(file).unwrap();
    let mut lines = text.lines();
    let expected_head: ContentId = lines.next().unwrap().parse().unwrap();
    let before_view: ContentId = lines.next().unwrap().parse().unwrap();
    let target_view: ContentId = lines.next().unwrap().parse().unwrap();
    let command = BString::from(lines.next().unwrap());
    let count: usize = lines.next().unwrap().parse().unwrap();
    let mut actions = Vec::with_capacity(count);
    for _ in 0..count {
        let parts: Vec<&str> = lines.next().unwrap().split('|').collect();
        let action = match parts[0] {
            "rehydrate" => Action::RehydrateObject {
                oid: parts[1].parse().unwrap(),
                content: parts[2].parse().unwrap(),
            },
            "write-wt" => Action::WriteWorktreeEntry {
                path: BString::from(hex_decode(parts[1])),
                expected_before: if parts[2] == "none" {
                    None
                } else {
                    Some(parse_entry_state(parts[2]))
                },
                target: parse_entry_state(parts[3]),
            },
            "rm-wt" => Action::RemoveWorktreeEntry {
                path: BString::from(hex_decode(parts[1])),
                expected_before: parse_entry_state(parts[2]),
            },
            "ensure-dir" => Action::EnsureDir {
                path: BString::from(hex_decode(parts[1])),
            },
            "rm-dir" => Action::RemoveEmptyDir {
                path: BString::from(hex_decode(parts[1])),
            },
            "write-idx" => Action::WriteIndex {
                expected_before: if parts[1] == "none" {
                    None
                } else {
                    Some(parts[1].parse().unwrap())
                },
                target: parts[2].parse().unwrap(),
            },
            "rm-idx" => Action::RemoveIndex {
                expected_before: parts[1].parse().unwrap(),
            },
            "write-ref" => Action::WriteRef {
                name: BString::from(hex_decode(parts[1])),
                expected_before: if parts[2] == "none" {
                    None
                } else {
                    Some(BString::from(hex_decode(parts[2])))
                },
                target: BString::from(hex_decode(parts[3])),
            },
            "rm-ref" => Action::RemoveRef {
                name: BString::from(hex_decode(parts[1])),
                expected_before: BString::from(hex_decode(parts[2])),
            },
            "write-packed" => Action::WritePackedRefs {
                expected_before: if parts[1] == "none" {
                    None
                } else {
                    Some(BString::from(hex_decode(parts[1])))
                },
                target: if parts[2] == "none" {
                    None
                } else {
                    Some(BString::from(hex_decode(parts[2])))
                },
            },
            "write-head" => Action::WriteHead {
                expected_before: BString::from(hex_decode(parts[1])),
                target: BString::from(hex_decode(parts[2])),
            },
            other => panic!("unknown action {other}"),
        };
        actions.push(action);
    }
    MutationPlan {
        expected_head,
        before_view,
        target_view,
        command,
        actions,
    }
}

fn kind_str(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Regular => "reg",
        FileKind::Executable => "exec",
        FileKind::Symlink => "sym",
    }
}

fn parse_entry_state(s: &str) -> EntryState {
    let (k, c) = s.split_once(':').unwrap();
    let kind = match k {
        "reg" => FileKind::Regular,
        "exec" => FileKind::Executable,
        "sym" => FileKind::Symlink,
        _ => panic!("unknown kind {k}"),
    };
    EntryState {
        kind,
        content: c.parse().unwrap(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

fn hex_decode(s: &str) -> Vec<u8> {
    s.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn transaction_child_runner() {
    let Some(mode) = std::env::var_os("GIT2_TX_CHILD_MODE") else {
        return;
    };
    let repo_path = std::env::var_os("GIT2_TX_CHILD_REPO").unwrap();
    let failpoint = parse_failpoint(
        &std::env::var("GIT2_TX_CHILD_FAILPOINT").expect("GIT2_TX_CHILD_FAILPOINT"),
    );
    match mode.to_string_lossy().as_ref() {
        "run" => {
            let plan_file = std::env::var_os("GIT2_TX_CHILD_PLAN").unwrap();
            let plan = read_plan_fixture(Path::new(&plan_file));
            let repo = LocalRepository::discover(repo_path).unwrap();
            run_with_hook(&repo, &plan, |fp| {
                if fp == failpoint {
                    std::process::abort();
                }
                Ok(())
            })
            .unwrap();
        }
        "recover" => {
            let rec = RecoveryRepository::open(repo_path).unwrap();
            recover_with_hook(&rec, |fp| {
                if fp == failpoint {
                    std::process::abort();
                }
                Ok(())
            })
            .unwrap();
        }
        other => panic!("unknown child mode {other}"),
    }
}

#[test]
fn noop_synthetic_mutation_produces_valid_operation() {
    let dir = init_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let adoption = repo.adopt(AdoptOptions::default()).unwrap();

    let plan = MutationPlan {
        expected_head: adoption.operation_id,
        before_view: adoption.view_id,
        target_view: adoption.view_id,
        command: BString::from("synthetic-noop"),
        actions: Vec::new(),
    };
    let op_id = run(&repo, &plan).unwrap();
    assert_ne!(op_id, adoption.operation_id);

    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    assert_eq!(store.head().unwrap(), op_id);
    let op = store.read_operation(op_id).unwrap();
    assert_eq!(op.parent, Some(adoption.operation_id));
    assert_eq!(op.before, adoption.view_id);
    assert_eq!(op.after, adoption.view_id);
    assert_eq!(
        op.kind,
        OperationKind::Git2(BString::from("synthetic-noop"))
    );

    let rec = RecoveryRepository::open(dir.path()).unwrap();
    assert!(pending(&rec).unwrap().is_none());
    assert_eq!(recover(&rec).unwrap(), RecoveryOutcome::Clean);
}

#[test]
fn stale_plan_and_lock_contention_refuse_without_partial_mutation() {
    let dir = init_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let adoption = repo.adopt(AdoptOptions::default()).unwrap();

    // 1. Stale `expected_head`
    let stale_head_plan = MutationPlan {
        expected_head: ContentId::for_bytes(b"wrong head"),
        before_view: adoption.view_id,
        target_view: adoption.view_id,
        command: BString::from("synthetic-noop"),
        actions: Vec::new(),
    };
    assert!(run(&repo, &stale_head_plan).is_err());

    // 2. Active recovery lock contention fails immediately without touching `pending`
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let held_lock = store.try_lock().unwrap();
    let valid_plan = MutationPlan {
        expected_head: adoption.operation_id,
        before_view: adoption.view_id,
        target_view: adoption.view_id,
        command: BString::from("synthetic-noop"),
        actions: Vec::new(),
    };
    assert!(run(&repo, &valid_plan).is_err());
    assert!(!store.has_pending().unwrap());
    drop(held_lock);

    // 3. Stale `before_view` after external edit refuses before writing
    fs::write(dir.path().join("base.txt"), b"outside edit\n").unwrap();
    assert!(run(&repo, &valid_plan).is_err());
    assert_eq!(store.head().unwrap(), adoption.operation_id);

    // 4. Plan whose `before_view` matches live outside edit auto-imports `external-git`
    let draft_after_edit = repo.checkpoint(); // wait: capture view without changing HEAD?
    drop(draft_after_edit);
    let outside_view_id = store.current_view().unwrap();
    let ops = oplog_ids(&store);
    assert_eq!(ops.len(), 2);
    assert_eq!(
        ops[0].1.kind,
        OperationKind::Git2(BString::from("checkpoint"))
    );

    // Make another outside edit and capture only into CAS/views via a fresh repo to test auto-import
    fs::write(dir.path().join("base.txt"), b"second outside edit\n").unwrap();
    let new_bytes_id = store.put_content(b"second outside edit\n").unwrap();
    let mut next_view = store.read_view(outside_view_id).unwrap();
    next_view
        .files
        .iter_mut()
        .find(|f| f.path.as_slice() == b"base.txt")
        .unwrap()
        .content = new_bytes_id;
    let next_view_id = store.put_view(&next_view).unwrap();
    let ext_plan = MutationPlan {
        expected_head: store.head().unwrap(),
        before_view: next_view_id,
        target_view: next_view_id,
        command: BString::from("synthetic-after-external"),
        actions: Vec::new(),
    };
    let head_before = store.head().unwrap();
    let op_id = run(&repo, &ext_plan).unwrap();
    let op = store.read_operation(op_id).unwrap();
    let ext_op_id = op.parent.unwrap();
    let ext_op = store.read_operation(ext_op_id).unwrap();
    assert_eq!(ext_op.parent, Some(head_before));
    assert_eq!(ext_op.kind, OperationKind::ExternalGit);
    assert_eq!(ext_op.after, next_view_id);
}

#[test]
fn owned_lock_recovers_after_link_before_cursor() {
    let dir = init_repo();
    let fix = setup_multi_fixture(dir.path());
    let plan_tmp = tempfile::tempdir().unwrap();
    let plan_file = plan_tmp.path().join("plan.txt");
    write_plan_fixture(&plan_file, &fix.plan);

    let index_idx = fix
        .plan
        .actions
        .iter()
        .position(|a| matches!(a, Action::WriteIndex { .. }))
        .unwrap();
    let ref_idx = fix
        .plan
        .actions
        .iter()
        .position(|a| matches!(a, Action::WriteRef { .. }))
        .unwrap();

    for (idx, lock_rel) in [
        (index_idx, ".git/index.lock"),
        (ref_idx, ".git/refs/heads/feature.lock"),
    ] {
        // Re-create a fresh repo for each case
        let case_dir = init_repo();
        let case_fix = setup_multi_fixture(case_dir.path());
        let case_plan_file = plan_tmp.path().join(format!("plan_{idx}.txt"));
        write_plan_fixture(&case_plan_file, &case_fix.plan);

        let output = spawn_child_abort(
            case_dir.path(),
            "run",
            Failpoint::AfterLockPublish(idx),
            &case_plan_file,
        );
        assert!(!output.status.success(), "child must abort at lock publish");
        assert!(
            case_dir.path().join(lock_rel).is_file(),
            "owned lock {lock_rel} must exist after crash at AfterLockPublish({idx})"
        );

        let rec = RecoveryRepository::open(case_dir.path()).unwrap();
        let summary = pending(&rec).unwrap().expect("pending must exist");
        assert_eq!(summary.phase, PendingPhase::Applying);
        assert_eq!(summary.next_action, idx);

        let first = recover(&rec).unwrap();
        let RecoveryOutcome::Finalized {
            operation_id,
            after_view,
        } = first
        else {
            panic!("expected Finalized, got {first:?}");
        };
        assert_eq!(after_view, case_fix.plan.target_view);
        assert!(!case_dir.path().join(lock_rel).exists());
        assert_eq!(recover(&rec).unwrap(), RecoveryOutcome::Clean);
        assert!(pending(&rec).unwrap().is_none());

        assert_eq!(
            fs::read(case_dir.path().join(".git/index")).unwrap(),
            case_fix.new_index_bytes
        );
        assert_eq!(
            fs::read_to_string(case_dir.path().join(".git/refs/heads/feature")).unwrap(),
            format!("{}\n", case_fix.new_commit_hex)
        );
        assert_eq!(
            fs::read_to_string(case_dir.path().join(".git/HEAD")).unwrap(),
            "ref: refs/heads/feature\n"
        );
        assert_eq!(rec.store().head().unwrap(), operation_id);
        git(case_dir.path(), &["fsck", "--full"]);
    }
    let _ = fs::remove_file(plan_file);
}

#[test]
fn unknown_lock_remains_byte_identical() {
    let dir = init_repo();
    let repo = LocalRepository::discover(dir.path()).unwrap();
    let adoption = repo.adopt(AdoptOptions::default()).unwrap();
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let view = store.read_view(adoption.view_id).unwrap();
    let idx_id = view.index.unwrap();

    // 1. Foreign `.git/index.lock` before `run`
    let foreign_bytes = b"foreign-git-lock-sentinel";
    let index_lock = dir.path().join(".git/index.lock");
    fs::write(&index_lock, foreign_bytes).unwrap();
    let plan = MutationPlan {
        expected_head: adoption.operation_id,
        before_view: adoption.view_id,
        target_view: adoption.view_id,
        command: BString::from("synthetic-lock"),
        actions: vec![Action::WriteIndex {
            expected_before: Some(idx_id),
            target: idx_id,
        }],
    };
    // Make a slightly different target so `actual != target` and lock is attempted
    let mut alt_index = store.read_content(idx_id).unwrap();
    alt_index.push(b'x'); // only used if not checked as git index; WriteIndex stores raw bytes in CAS
    let alt_id = store.put_content(&alt_index).unwrap();
    let mut alt_view = view.clone();
    alt_view.index = Some(alt_id);
    let alt_view_id = store.put_view(&alt_view).unwrap();
    let mut plan_diff = plan;
    plan_diff.target_view = alt_view_id;
    plan_diff.actions = vec![Action::WriteIndex {
        expected_before: Some(idx_id),
        target: alt_id,
    }];
    let err = run(&repo, &plan_diff).unwrap_err();
    assert!(err.to_string().contains("unknown Git lock"), "{err}");
    assert_eq!(fs::read(&index_lock).unwrap(), foreign_bytes);
    fs::remove_file(&index_lock).unwrap();
    let rec_cleanup = RecoveryRepository::open(dir.path()).unwrap();
    assert!(matches!(
        rollback(&rec_cleanup).unwrap(),
        RecoveryOutcome::RolledBack { .. }
    ));

    // 2. Foreign lock replaces owned lock while pending -> `recover` and `rollback` both refuse untouched
    let plan_tmp = tempfile::tempdir().unwrap();
    let plan_file = plan_tmp.path().join("plan.txt");
    write_plan_fixture(&plan_file, &plan_diff);
    let output = spawn_child_abort(
        dir.path(),
        "run",
        Failpoint::AfterLockPublish(0),
        &plan_file,
    );
    assert!(!output.status.success());
    assert!(index_lock.is_file());

    // Replace `.git/index.lock` with a fresh inode owned by a foreign process
    fs::remove_file(&index_lock).unwrap();
    fs::write(&index_lock, foreign_bytes).unwrap();

    let rec = RecoveryRepository::open(dir.path()).unwrap();
    let err_rec = recover(&rec).unwrap_err();
    assert!(
        err_rec.to_string().contains("unknown Git lock"),
        "{err_rec}"
    );
    assert_eq!(fs::read(&index_lock).unwrap(), foreign_bytes);

    let err_rb = rollback(&rec).unwrap_err();
    assert!(err_rb.to_string().contains("unknown Git lock"), "{err_rb}");
    assert_eq!(fs::read(&index_lock).unwrap(), foreign_bytes);
}

#[test]
fn synthetic_operation_resolves_once_after_head_selection() {
    let dir = init_repo();
    let fix = setup_multi_fixture(dir.path());
    let plan_tmp = tempfile::tempdir().unwrap();
    let plan_file = plan_tmp.path().join("plan.txt");
    write_plan_fixture(&plan_file, &fix.plan);

    // Child 1 dies right after moving recovery HEAD (before Committed/cleanup)
    let out1 = spawn_child_abort(dir.path(), "run", Failpoint::AfterHeadSelection, &plan_file);
    assert!(!out1.status.success());

    let rec = RecoveryRepository::open(dir.path()).unwrap();
    let summary = pending(&rec).unwrap().unwrap();
    let expected_op = summary.operation_id;
    assert_eq!(rec.store().head().unwrap(), expected_op);
    let count_before = oplog_ids(rec.store()).len();

    // Child 2 dies during `recover` right after confirming HEAD selection (simulated via RecoverAfterHeadSelection on a still-unselected? Wait: if HEAD already moved, `recover` returns before hook. So also test double-crash at RecoverAfterHeadSelection from AfterOperationWrite!)
    drop(rec);
    let dir2 = init_repo();
    let fix2 = setup_multi_fixture(dir2.path());
    let plan_file2 = plan_tmp.path().join("plan2.txt");
    write_plan_fixture(&plan_file2, &fix2.plan);
    let run_crash = spawn_child_abort(
        dir2.path(),
        "run",
        Failpoint::AfterOperationWrite,
        &plan_file2,
    );
    assert!(!run_crash.status.success());
    let recover_crash = spawn_child_abort(
        dir2.path(),
        "recover",
        Failpoint::RecoverAfterHeadSelection,
        &plan_file2,
    );
    assert!(!recover_crash.status.success());

    let rec2 = RecoveryRepository::open(dir2.path()).unwrap();
    let summary2 = pending(&rec2).unwrap().unwrap();
    let outcome = recover(&rec2).unwrap();
    assert_eq!(
        outcome,
        RecoveryOutcome::Finalized {
            operation_id: summary2.operation_id,
            after_view: fix2.plan.target_view,
        }
    );
    assert_eq!(recover(&rec2).unwrap(), RecoveryOutcome::Clean);
    assert!(pending(&rec2).unwrap().is_none());

    let rec1 = RecoveryRepository::open(dir.path()).unwrap();
    assert_eq!(
        recover(&rec1).unwrap(),
        RecoveryOutcome::Finalized {
            operation_id: expected_op,
            after_view: fix.plan.target_view,
        }
    );
    assert_eq!(oplog_ids(rec1.store()).len(), count_before);
    assert_eq!(recover(&rec1).unwrap(), RecoveryOutcome::Clean);
    git(dir.path(), &["fsck", "--full"]);
    git(dir2.path(), &["fsck", "--full"]);
}

#[test]
fn io_failure_leaves_durable_pre_view() {
    let dir = init_repo();
    let fix = setup_multi_fixture(dir.path());
    let repo = LocalRepository::discover(dir.path()).unwrap();

    // Inject an I/O failure at `BeforeFinalize` (all actions ran, `HEAD` has not moved)
    let err = run_with_hook(&repo, &fix.plan, |fp| {
        if fp == Failpoint::BeforeFinalize {
            return Err(Error::Io(io::Error::other("injected disk error")));
        }
        Ok(())
    })
    .unwrap_err();
    assert!(err.to_string().contains("injected disk error"));

    let rec = RecoveryRepository::open(dir.path()).unwrap();
    let summary = pending(&rec).unwrap().unwrap();
    assert_eq!(summary.phase, PendingPhase::Finalizing);
    assert_eq!(summary.before_view, fix.plan.before_view);
    assert_eq!(rec.store().head().unwrap(), fix.plan.expected_head);
    assert!(rec.store().read_view(fix.plan.before_view).is_ok());

    // Rollback cleanly restores the exact pre-view
    assert_eq!(
        rollback(&rec).unwrap(),
        RecoveryOutcome::RolledBack {
            before_view: fix.plan.before_view,
        }
    );
    assert!(pending(&rec).unwrap().is_none());
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        fix.before_index_bytes
    );
    assert_eq!(
        fs::read(dir.path().join(".git/HEAD")).unwrap(),
        fix.before_head_bytes
    );
    assert_eq!(
        fs::read(dir.path().join("remove.txt")).unwrap(),
        b"to remove\n"
    );
    assert!(dir.path().join("gone_dir").is_dir());
    assert!(!dir.path().join("nested/new.sh").exists());
    assert_eq!(
        fs::read(dir.path().join("ignored.txt")).unwrap(),
        b"must stay untouched\n"
    );
    git(dir.path(), &["fsck", "--full"]);

    // Second fixture: inject I/O error mid-apply, then recover forward once I/O clears
    let dir2 = init_repo();
    let fix2 = setup_multi_fixture(dir2.path());
    let repo2 = LocalRepository::discover(dir2.path()).unwrap();
    let write_idx = fix2
        .plan
        .actions
        .iter()
        .position(|a| matches!(a, Action::WriteWorktreeEntry { .. }))
        .unwrap();
    let err2 = run_with_hook(&repo2, &fix2.plan, |fp| {
        if fp == Failpoint::AfterAction(write_idx) {
            return Err(Error::Io(io::Error::other("injected mid-apply error")));
        }
        Ok(())
    })
    .unwrap_err();
    assert!(err2.to_string().contains("injected mid-apply error"));

    let rec2 = RecoveryRepository::open(dir2.path()).unwrap();
    assert!(matches!(
        recover(&rec2).unwrap(),
        RecoveryOutcome::Finalized { .. }
    ));
    assert_eq!(recover(&rec2).unwrap(), RecoveryOutcome::Clean);
    git(dir2.path(), &["fsck", "--full"]);
}

#[test]
fn process_kill_matrix_recovers_every_boundary() {
    let sample_dir = init_repo();
    let sample = setup_multi_fixture(sample_dir.path());
    let mut failpoints = vec![Failpoint::BeforePendingWrite, Failpoint::AfterPrepare];
    for (idx, action) in sample.plan.actions.iter().enumerate() {
        match action {
            Action::WriteIndex { .. }
            | Action::RemoveIndex { .. }
            | Action::WriteRef { .. }
            | Action::RemoveRef { .. }
            | Action::WritePackedRefs { .. }
            | Action::WriteHead { .. } => {
                failpoints.push(Failpoint::BeforeLockPublish(idx));
                failpoints.push(Failpoint::AfterLockPublish(idx));
                failpoints.push(Failpoint::AfterAction(idx));
            }
            _ => {
                failpoints.push(Failpoint::BeforeAction(idx));
                failpoints.push(Failpoint::AfterAction(idx));
            }
        }
    }
    failpoints.extend([
        Failpoint::BeforeFinalize,
        Failpoint::AfterOperationWrite,
        Failpoint::AfterHeadSelection,
        Failpoint::BeforePendingCleanup,
    ]);

    let plan_tmp = tempfile::tempdir().unwrap();
    for (case_num, fp) in failpoints.into_iter().enumerate() {
        verify_matrix_case(plan_tmp.path(), case_num, fp);
    }
}

fn verify_matrix_case(plan_dir: &Path, case_num: usize, fp: Failpoint) {
    let dir = init_repo();
    let fix = setup_multi_fixture(dir.path());
    let plan_file = plan_dir.join(format!("matrix_{case_num}.txt"));
    write_plan_fixture(&plan_file, &fix.plan);

    let store_before = RecoveryStore::open(dir.path().join(".git")).unwrap();
    let ops_before = oplog_ids(&store_before).len();

    let out = spawn_child_abort(dir.path(), "run", fp, &plan_file);
    assert!(!out.status.success(), "child must abort at {fp:?}");

    let rec = RecoveryRepository::open(dir.path()).unwrap();
    let outcome = recover(&rec).unwrap();
    match fp {
        Failpoint::BeforePendingWrite => {
            assert_eq!(outcome, RecoveryOutcome::Clean);
            assert_eq!(oplog_ids(rec.store()).len(), ops_before);
            assert_eq!(
                fs::read(dir.path().join("remove.txt")).unwrap(),
                b"to remove\n"
            );
        }
        Failpoint::AfterPrepare => {
            assert_eq!(
                outcome,
                RecoveryOutcome::Aborted {
                    before_view: fix.plan.before_view,
                }
            );
            assert_eq!(oplog_ids(rec.store()).len(), ops_before);
            assert_eq!(rec.store().head().unwrap(), fix.plan.expected_head);
            assert_eq!(
                fs::read(dir.path().join("remove.txt")).unwrap(),
                b"to remove\n"
            );
        }
        _ => {
            let RecoveryOutcome::Finalized {
                operation_id,
                after_view,
            } = outcome
            else {
                panic!("expected Finalized at {fp:?}, got {outcome:?}");
            };
            assert_eq!(after_view, fix.plan.target_view);
            assert_eq!(rec.store().head().unwrap(), operation_id);
            assert_eq!(oplog_ids(rec.store()).len(), ops_before + 1);
            assert!(!dir.path().join("remove.txt").exists());
            assert!(!dir.path().join("gone_dir").exists());
            assert!(dir.path().join("nested/new.sh").is_file());
            let meta = fs::metadata(dir.path().join("nested/new.sh")).unwrap();
            assert_ne!(meta.permissions().mode() & 0o111, 0);
            assert_eq!(
                fs::read(dir.path().join(".git/index")).unwrap(),
                fix.new_index_bytes
            );
            assert!(
                git_cmd(dir.path(), &["cat-file", "-e", &fix.pruned_oid.to_hex()])
                    .status
                    .success(),
                "pruned staged object must be rehydrated at {fp:?}"
            );
            let obj = ObjectStore::open(dir.path())
                .unwrap()
                .read(fix.pruned_oid)
                .unwrap();
            assert_eq!(obj.kind, ObjectKind::Blob);
            assert_eq!(
                parse_canonical_object(&obj.canonical_bytes()).unwrap().id(),
                fix.pruned_oid
            );
        }
    }
    assert_eq!(recover(&rec).unwrap(), RecoveryOutcome::Clean);
    assert!(pending(&rec).unwrap().is_none());
    assert_eq!(
        fs::read(dir.path().join("ignored.txt")).unwrap(),
        b"must stay untouched\n"
    );
    assert_eq!(
        fs::read_dir(dir.path().join(".git/git2/pending"))
            .unwrap()
            .count(),
        0,
        "pending directory must be empty after recovery at {fp:?}"
    );
    assert!(!dir.path().join(".git/index.lock").exists());
    assert!(!dir.path().join(".git/HEAD.lock").exists());
    git(dir.path(), &["fsck", "--full"]);
}

#[test]
fn corrupt_pending_records_are_refused_without_git_writes() {
    let dir = init_repo();
    let fix = setup_multi_fixture(dir.path());
    let plan_tmp = tempfile::tempdir().unwrap();
    let plan_file = plan_tmp.path().join("plan.txt");
    write_plan_fixture(&plan_file, &fix.plan);

    let out = spawn_child_abort(dir.path(), "run", Failpoint::AfterPrepare, &plan_file);
    assert!(!out.status.success());

    let pending_dir = dir.path().join(".git/git2/pending");
    let orig_active = fs::read(pending_dir.join("active")).unwrap();
    let orig_plan = fs::read(pending_dir.join("plan")).unwrap();
    let rec = RecoveryRepository::open(dir.path()).unwrap();

    // 1. Truncated active
    fs::write(
        pending_dir.join("active"),
        &orig_active[..orig_active.len() - 1],
    )
    .unwrap();
    assert!(matches!(pending(&rec), Err(Error::CorruptRecovery(_))));
    assert!(matches!(recover(&rec), Err(Error::CorruptRecovery(_))));
    assert_eq!(
        fs::read(dir.path().join(".git/index")).unwrap(),
        fix.before_index_bytes
    );

    // 2. Trailing byte in active
    let mut trailing = orig_active.clone();
    trailing.push(0);
    fs::write(pending_dir.join("active"), &trailing).unwrap();
    assert!(matches!(recover(&rec), Err(Error::CorruptRecovery(_))));

    // 3. Mismatched plan bytes
    fs::write(pending_dir.join("active"), &orig_active).unwrap();
    let mut bad_plan = orig_plan.clone();
    bad_plan[0] ^= 0xff;
    fs::write(pending_dir.join("plan"), &bad_plan).unwrap();
    assert!(matches!(pending(&rec), Err(Error::CorruptRecovery(_))));
    assert!(matches!(recover(&rec), Err(Error::CorruptRecovery(_))));

    // 4. Missing plan file
    fs::remove_file(pending_dir.join("plan")).unwrap();
    assert!(matches!(pending(&rec), Err(Error::CorruptRecovery(_))));
    assert!(matches!(recover(&rec), Err(Error::CorruptRecovery(_))));

    // Restore valid pending with private permissions and verify recovery still succeeds cleanly
    fs::write(pending_dir.join("plan"), &orig_plan).unwrap();
    fs::set_permissions(pending_dir.join("plan"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        recover(&rec).unwrap(),
        RecoveryOutcome::Aborted { .. }
    ));
}

#[test]
fn pending_transaction_blocks_checkpoint_and_external_import() {
    let dir = init_repo();
    let fix = setup_multi_fixture(dir.path());
    let plan_tmp = tempfile::tempdir().unwrap();
    let plan_file = plan_tmp.path().join("plan.txt");
    write_plan_fixture(&plan_file, &fix.plan);

    let out = spawn_child_abort(dir.path(), "run", Failpoint::AfterPrepare, &plan_file);
    assert!(!out.status.success());

    let repo = LocalRepository::discover(dir.path()).unwrap();
    assert!(
        repo.checkpoint()
            .unwrap_err()
            .to_string()
            .contains("pending transaction")
    );
    assert!(
        repo.import_external_changes()
            .unwrap_err()
            .to_string()
            .contains("pending transaction")
    );

    let rec = RecoveryRepository::open(dir.path()).unwrap();
    assert!(matches!(
        recover(&rec).unwrap(),
        RecoveryOutcome::Aborted { .. }
    ));
    assert!(repo.checkpoint().is_ok());
}
