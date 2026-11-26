#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use git2_core::object::{GitOid, parse_canonical_object};
use git2_core::recovery::{Operation, OperationKind, RecoveryStore, View};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::retention;

fn command(path: &Path, args: &[&str]) -> Output {
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
    let output = command(path, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn oid(path: &Path, revision: &str) -> GitOid {
    git(path, &["rev-parse", "--verify", revision])
        .trim()
        .parse()
        .unwrap()
}

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    git(
        directory.path(),
        &["init", "--quiet", "-b", "main", "--object-format=sha1"],
    );
    directory
}

fn commit(path: &Path, bytes: &[u8], name: &str) {
    fs::write(path.join("file"), bytes).unwrap();
    git(path, &["add", "file"]);
    git(path, &["commit", "-q", "-m", name]);
}

fn adopt(path: &Path) -> (RecoveryStore, View) {
    let repo = LocalRepository::discover(path).unwrap();
    let adoption = repo.adopt(AdoptOptions::default()).unwrap();
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let view = store.read_view(adoption.view_id).unwrap();
    (store, view)
}

fn assert_retained(store: &RecoveryStore, view: &View, id: GitOid) {
    let entry = view
        .retained_objects
        .iter()
        .find(|entry| entry.oid == id)
        .unwrap_or_else(|| panic!("missing retained Git object {id}"));
    let canonical = store.read_content(entry.content).unwrap();
    assert_eq!(parse_canonical_object(&canonical).unwrap().id(), id);
}

#[test]
fn retains_shadowed_packed_ref_and_staged_blob() {
    let directory = repository();
    let path = directory.path();
    let repo = LocalRepository::discover(path).unwrap();
    let baseline = repo.adopt(AdoptOptions::default()).unwrap();
    commit(path, b"base\n", "packed old root");
    let shadowed = oid(path, "HEAD");
    git(path, &["pack-refs", "--all", "--prune"]);
    let independent = git(
        path,
        &["commit-tree", "HEAD^{tree}", "-m", "independent root"],
    );
    git(path, &["update-ref", "refs/heads/main", independent.trim()]);
    fs::write(path.join("file"), b"unique staged-only bytes\n").unwrap();
    git(path, &["add", "file"]);
    let staged = oid(path, ":file");
    fs::write(path.join("file"), b"other worktree bytes\n").unwrap();

    let saved_refs = git(path, &["show-ref"]);
    let ordinary_head = fs::read(path.join(".git/HEAD")).unwrap();
    let ordinary_index = fs::read(path.join(".git/index")).unwrap();
    let ordinary_objects = git(
        path,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    let checkpoint = repo.checkpoint().unwrap();
    assert!(checkpoint.changed);
    assert_ne!(baseline.view_id, checkpoint.view_id);
    assert_eq!(git(path, &["show-ref"]), saved_refs);
    assert_eq!(fs::read(path.join(".git/HEAD")).unwrap(), ordinary_head);
    assert_eq!(fs::read(path.join(".git/index")).unwrap(), ordinary_index);
    assert_eq!(
        git(
            path,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)"
            ]
        ),
        ordinary_objects
    );
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    let view = store.read_view(checkpoint.view_id).unwrap();
    assert!(
        view.refs.iter().any(|(name, contents)| {
            name.as_slice() == b"packed-refs"
                && contents
                    .windows(40)
                    .any(|window| window == shadowed.to_hex().as_bytes())
        }),
        "the fixture must actually contain the shadowed packed ref"
    );
    git(path, &["pack-refs", "--all", "--prune"]);
    git(path, &["reset", "--mixed", "HEAD"]);
    git(path, &["reflog", "expire", "--expire=now", "--all"]);
    git(path, &["gc", "--quiet", "--prune=now"]);
    for id in [shadowed, staged] {
        assert!(
            !command(path, &["cat-file", "-e", &id.to_hex()])
                .status
                .success(),
            "Git still has object {id} after pruning"
        );
        assert_retained(&store, &view, id);
    }
    retention::verify(&store, &view).unwrap();
    let mut incomplete = view;
    incomplete
        .retained_objects
        .retain(|entry| entry.oid != staged);
    assert!(retention::verify(&store, &incomplete).is_err());
}

#[test]
fn retains_detached_head_tag_parent_tree_closure() {
    let directory = repository();
    let path = directory.path();
    commit(path, b"parent\n", "parent");
    let parent = oid(path, "HEAD");
    let parent_tree = oid(path, "HEAD^{tree}");
    let parent_blob = oid(path, "HEAD:file");
    git(path, &["tag", "-am", "first", "first"]);
    git(path, &["pack-refs", "--all", "--prune"]);
    git(path, &["checkout", "--detach", "-q"]);
    commit(path, b"child\n", "child");
    let detached = oid(path, "HEAD");
    let tag = oid(path, "refs/tags/first");
    git(path, &["gc", "--quiet", "--prune=now"]);

    let (store, view) = adopt(path);
    for id in [detached, tag, parent, parent_tree, parent_blob] {
        assert_retained(&store, &view, id);
    }
    assert!(
        view.retained_objects
            .windows(2)
            .all(|pair| pair[0].oid < pair[1].oid)
    );
    retention::verify(&store, &view).unwrap();
}

#[test]
fn unborn_head_is_not_a_missing_object() {
    let directory = repository();
    let path = directory.path();
    fs::write(path.join("file"), b"worktree, not staged").unwrap();
    git(path, &["add", "--intent-to-add", "file"]);
    let (_, view) = adopt(path);
    assert!(view.retained_objects.is_empty());
}

#[test]
fn rejects_missing_required_object() {
    let directory = repository();
    let path = directory.path();
    fs::write(path.join("file"), b"will lose staged coverage").unwrap();
    git(path, &["add", "file"]);
    let id = oid(path, ":file");
    let repo = LocalRepository::discover(path).unwrap();
    let hex = id.to_hex();
    fs::remove_file(
        repo.git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]),
    )
    .unwrap();

    let error = repo.adopt(AdoptOptions::default()).unwrap_err();
    assert!(error.to_string().contains(&hex), "{error}");
    assert!(!repo.git_dir().join("git2/HEAD").exists());
}

#[test]
fn failed_checkpoint_keeps_the_last_complete_view_selected() {
    let directory = repository();
    let path = directory.path();
    let repo = LocalRepository::discover(path).unwrap();
    let baseline = repo.adopt(AdoptOptions::default()).unwrap();
    fs::write(path.join("file"), b"staged object removed from fixture").unwrap();
    git(path, &["add", "file"]);
    let hex = oid(path, ":file").to_hex();
    fs::remove_file(
        repo.git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..]),
    )
    .unwrap();

    assert!(repo.checkpoint().unwrap_err().to_string().contains(&hex));
    let store = RecoveryStore::open(repo.git_dir()).unwrap();
    assert_eq!(store.head().unwrap(), baseline.operation_id);
    assert_eq!(store.current_view().unwrap(), baseline.view_id);
    retention::verify(&store, &store.read_view(baseline.view_id).unwrap()).unwrap();
}

#[test]
fn keeps_cache_tree_objects_that_are_not_reachable_from_head() {
    let directory = repository();
    let path = directory.path();
    fs::write(path.join("file"), b"staged before first commit").unwrap();
    git(path, &["add", "file"]);
    let tree = git(path, &["write-tree"]).trim().parse().unwrap();
    let (store, view) = adopt(path);
    assert_retained(&store, &view, tree);
    retention::verify(&store, &view).unwrap();
}

#[test]
fn captures_a_new_checkpoint_without_rewriting_earlier_v2_views() {
    let directory = repository();
    let path = directory.path();
    commit(path, b"base\n", "base");
    let (store, mut old_view) = adopt(path);
    old_view.retained_objects.clear();
    let old_view_id = store.put_view(&old_view).unwrap();
    let old_operation_id = store
        .put_operation(&Operation {
            parent: None,
            before: old_view_id,
            after: old_view_id,
            kind: OperationKind::Adopt,
            timestamp_seconds: 0,
        })
        .unwrap();
    store.set_head(old_operation_id).unwrap();

    let repo = LocalRepository::discover(path).unwrap();
    let checkpoint = repo.checkpoint().unwrap();
    assert!(checkpoint.changed);
    assert_ne!(checkpoint.view_id, old_view_id);
    assert_eq!(store.read_view(old_view_id).unwrap(), old_view);
    assert!(retention::verify(&store, &old_view).is_err());
    let new_view = store.read_view(checkpoint.view_id).unwrap();
    retention::verify(&store, &new_view).unwrap();
    assert_eq!(
        store
            .read_operation(checkpoint.operation_id)
            .unwrap()
            .parent,
        Some(old_operation_id)
    );
}

#[test]
fn retains_objects_from_a_deltified_pack() {
    let directory = repository();
    let path = directory.path();
    let mut payload: Vec<_> = (0_u16..8000).flat_map(u16::to_le_bytes).collect();
    let mut blobs = Vec::new();
    for n in 0..8 {
        payload[n * 1000] = b'@';
        commit(path, &payload, &format!("version {n}"));
        blobs.push(oid(path, "HEAD:file"));
    }
    git(path, &["repack", "-adf", "--window=10", "--depth=50"]);
    let index = fs::read_dir(path.join(".git/objects/pack"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .unwrap();
    let pack = git(path, &["verify-pack", "-v", index.to_str().unwrap()]);
    assert!(
        blobs.iter().any(|id| pack.lines().any(|line| {
            line.starts_with(&id.to_hex()) && line.split_whitespace().count() == 7
        })),
        "test must contain a deltified blob"
    );
    // An interrupted pack cleanup can leave an orphan index. It is not an object source.
    fs::copy(&index, index.with_file_name("pack-orphan.idx")).unwrap();
    let (store, view) = adopt(path);
    for id in blobs {
        assert_retained(&store, &view, id);
    }
    retention::verify(&store, &view).unwrap();
}
