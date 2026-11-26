#![cfg(unix)]

use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

use git2_core::Error;
use git2_core::recovery::{FileKind, FileRecord, RecoveryStore, View};
use git2_core::repository::{AdoptOptions, LocalRepository};

fn git_command(directory: &Path, arguments: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "git2 test")
        .env("GIT_AUTHOR_EMAIL", "git2@example.invalid")
        .env("GIT_COMMITTER_NAME", "git2 test")
        .env("GIT_COMMITTER_EMAIL", "git2@example.invalid");
    cmd
}

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = git_command(directory, arguments)
        .output()
        .expect("Git must be installed for adoption tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn repository() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("temporary repository");
    git(directory.path(), &["init", "--quiet"]);
    directory
}

fn commit(directory: &Path, message: &str) {
    git(
        directory,
        &[
            "-c",
            "user.name=git2 test",
            "-c",
            "user.email=git2@example.invalid",
            "commit",
            "--quiet",
            "-m",
            message,
        ],
    );
}

fn git_expect_failure(directory: &Path, arguments: &[&str]) {
    let output = git_command(directory, arguments)
        .output()
        .expect("Git must be installed for adoption tests");
    assert!(
        !output.status.success(),
        "git {arguments:?} unexpectedly succeeded"
    );
}

fn mkfifo(path: &Path) {
    let status = Command::new("mkfifo")
        .arg(path)
        .status()
        .expect("mkfifo must be installed for adoption tests");
    assert!(status.success());
}

fn view_file<'a>(view: &'a View, path: &[u8]) -> &'a FileRecord {
    view.files
        .iter()
        .find(|file| {
            let bytes: &[u8] = file.path.as_ref();
            bytes == path
        })
        .unwrap()
}

fn assert_refused_before_sidecar(label: &str, worktree: &Path, git_dir: &Path) {
    let location = worktree.display();
    let repository = match LocalRepository::discover(worktree) {
        Ok(repository) => repository,
        Err(error) => {
            assert!(
                matches!(error, Error::UnsupportedRepository(_)),
                "{label} ({location}): discovery must refuse with UnsupportedRepository, got {error:?}"
            );
            assert!(
                !git_dir.join("git2").exists(),
                "{label} ({location}): discovery refusal must not leave a sidecar"
            );
            return;
        }
    };
    let error = match repository.adopt(AdoptOptions::default()) {
        Err(error) => error,
        Ok(adoption) => panic!(
            "{label} ({location}): capture must refuse before the sidecar, but adopted {adoption:?}"
        ),
    };
    assert!(
        matches!(error, Error::UnsupportedRepository(_)),
        "{label} ({location}): capture must refuse with UnsupportedRepository, got {error:?}"
    );
    assert!(
        !repository.git_dir().join("git2").exists(),
        "{label} ({location}): capture refusal must not leave a sidecar"
    );
}

#[test]
fn adoption_captures_tracked_and_nonignored_untracked_files() {
    let directory = repository();
    std::fs::write(directory.path().join(".gitignore"), b"ignored.bin\n").unwrap();
    std::fs::write(
        directory.path().join("tracked.txt"),
        b"tracked before adoption\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("untracked.bin"), b"untracked\0bytes").unwrap();
    std::fs::write(
        directory.path().join("ignored.bin"),
        b"must not enter recovery",
    )
    .unwrap();
    git(directory.path(), &["add", ".gitignore", "tracked.txt"]);
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

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let adoption = repository.adopt(AdoptOptions::default()).unwrap();
    assert_eq!(adoption.file_count, 3);

    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    assert_eq!(store.current_view().unwrap(), adoption.view_id);
    let view = store.read_view(adoption.view_id).unwrap();
    let paths = view
        .files
        .iter()
        .map(|file| {
            let path: &[u8] = file.path.as_ref();
            path.to_vec()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            b".gitignore".to_vec(),
            b"tracked.txt".to_vec(),
            b"untracked.bin".to_vec()
        ]
    );
    assert!(view.index.is_some());
    assert!(!view.refs.is_empty());
    assert!(
        !view.files.iter().any(|file| {
            let path: &[u8] = file.path.as_ref();
            path == b"ignored.bin"
        }),
        "ignored files are outside the recovery guarantee"
    );
    let untracked = view
        .files
        .iter()
        .find(|file| {
            let path: &[u8] = file.path.as_ref();
            path == b"untracked.bin"
        })
        .unwrap();
    assert_eq!(
        store.read_content(untracked.content).unwrap(),
        b"untracked\0bytes"
    );
    assert!(
        git(directory.path(), &["status", "--porcelain"])
            .stderr
            .is_empty()
    );
}

#[test]
fn oversize_file_refuses_adoption_before_creating_sidecar() {
    let directory = repository();
    std::fs::write(directory.path().join("large.bin"), b"12345").unwrap();
    let repository = LocalRepository::discover(directory.path()).unwrap();
    let error = repository
        .adopt(AdoptOptions { max_file_bytes: 4 })
        .unwrap_err();
    assert!(matches!(error, Error::UnsupportedRepository(_)));
    assert!(!repository.git_dir().join("git2").exists());
}

#[test]
fn oversized_git_metadata_is_refused_before_creating_sidecar() {
    let directory = repository();
    let repository = LocalRepository::discover(directory.path()).unwrap();
    std::fs::File::create(repository.git_dir().join("packed-refs"))
        .unwrap()
        .set_len(16 * 1024 * 1024 + 1)
        .unwrap();

    let error = repository.adopt(AdoptOptions::default()).unwrap_err();
    assert!(matches!(error, Error::UnsupportedRepository(_)), "{error}");
    assert!(!repository.git_dir().join("git2").exists());
}

#[test]
fn in_progress_git_operation_is_refused() {
    let directory = repository();
    std::fs::write(
        directory.path().join(".git").join("MERGE_HEAD"),
        b"fixture\n",
    )
    .unwrap();
    let Err(error) = LocalRepository::discover(directory.path()) else {
        panic!("in-progress merge must be refused");
    };
    assert!(matches!(error, Error::UnsupportedRepository(_)));
}

#[test]
fn external_git_changes_append_one_operation() {
    let directory = repository();
    std::fs::write(directory.path().join("tracked"), b"one\n").unwrap();
    git(directory.path(), &["add", "tracked"]);

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let baseline = repository.adopt(AdoptOptions::default()).unwrap();
    std::fs::write(directory.path().join("tracked"), b"two\n").unwrap();
    std::fs::write(directory.path().join("new"), b"new\n").unwrap();

    let imported = repository.import_external_changes().unwrap();
    assert!(imported.changed);
    assert_ne!(imported.view_id, baseline.view_id);
    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    let operation = store.read_operation(imported.operation_id).unwrap();
    assert_eq!(operation.parent, Some(baseline.operation_id));
    assert_eq!(operation.before, baseline.view_id);
    assert_eq!(operation.after, imported.view_id);

    let unchanged = repository.import_external_changes().unwrap();
    assert!(!unchanged.changed);
    assert_eq!(unchanged.operation_id, imported.operation_id);
}

#[test]
fn persisted_file_limit_controls_later_import() {
    let directory = repository();
    let repository = LocalRepository::discover(directory.path()).unwrap();
    let baseline = repository
        .adopt(AdoptOptions { max_file_bytes: 4 })
        .unwrap();
    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    assert_eq!(store.policy().unwrap().max_file_bytes, 4);

    let worktree_file = directory.path().join("work.txt");
    std::fs::write(&worktree_file, b"12345").unwrap();
    assert!(matches!(
        repository.import_external_changes(),
        Err(Error::UnsupportedRepository(_))
    ));
    assert_eq!(store.head().unwrap(), baseline.operation_id);

    std::fs::write(worktree_file, b"1234").unwrap();
    assert!(repository.import_external_changes().unwrap().changed);
}

#[test]
fn adopts_files_in_plain_untracked_directories() {
    let directory = repository();
    let subdir = directory.path().join("plain");
    std::fs::create_dir(&subdir).unwrap();
    std::fs::write(subdir.join("file.txt"), b"untracked child").unwrap();
    let repository = LocalRepository::discover(directory.path()).unwrap();
    let adoption = repository.adopt(AdoptOptions::default()).unwrap();
    assert_eq!(adoption.file_count, 1);
    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    let view = store.read_view(adoption.view_id).unwrap();
    let file = view_file(&view, b"plain/file.txt");
    assert_eq!(
        store.read_content(file.content).unwrap(),
        b"untracked child"
    );
}

#[test]
fn captures_actual_mode_not_index_mode() {
    let directory = repository();
    let demoted = directory.path().join("demoted.txt");
    std::fs::write(&demoted, b"demoted\n").unwrap();
    std::fs::set_permissions(&demoted, std::fs::Permissions::from_mode(0o755)).unwrap();
    let promoted = directory.path().join("promoted.txt");
    std::fs::write(&promoted, b"promoted\n").unwrap();
    git(directory.path(), &["add", "demoted.txt", "promoted.txt"]);
    std::fs::set_permissions(&demoted, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&promoted, std::fs::Permissions::from_mode(0o755)).unwrap();

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let adoption = repository.adopt(AdoptOptions::default()).unwrap();
    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    let view = store.read_view(adoption.view_id).unwrap();

    let file = view_file(&view, b"demoted.txt");
    assert_eq!(file.kind, FileKind::Regular);
    assert_eq!(store.read_content(file.content).unwrap(), b"demoted\n");
    let file = view_file(&view, b"promoted.txt");
    assert_eq!(file.kind, FileKind::Executable);
    assert_eq!(store.read_content(file.content).unwrap(), b"promoted\n");
}

#[test]
fn captures_leaf_symlink_target_without_dereference() {
    let directory = repository();
    std::fs::write(directory.path().join(".gitignore"), b"ignored.bin\n").unwrap();
    std::fs::write(directory.path().join("ignored.bin"), b"stays out\n").unwrap();
    std::fs::write(directory.path().join("payload.txt"), b"regular payload\n").unwrap();
    symlink("payload.txt", directory.path().join("tracked-link")).unwrap();
    symlink("payload.txt", directory.path().join("untracked-link")).unwrap();
    git(
        directory.path(),
        &["add", ".gitignore", "payload.txt", "tracked-link"],
    );

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let adoption = repository.adopt(AdoptOptions::default()).unwrap();
    let store = RecoveryStore::open(repository.git_dir()).unwrap();
    let view = store.read_view(adoption.view_id).unwrap();

    let tracked = view_file(&view, b"tracked-link");
    assert_eq!(tracked.kind, FileKind::Symlink);
    assert_eq!(store.read_content(tracked.content).unwrap(), b"payload.txt");
    let untracked = view_file(&view, b"untracked-link");
    assert_eq!(untracked.kind, FileKind::Symlink);
    assert_eq!(
        store.read_content(untracked.content).unwrap(),
        b"payload.txt"
    );
    assert_eq!(
        store
            .read_content(view_file(&view, b"payload.txt").content)
            .unwrap(),
        b"regular payload\n"
    );
    let paths = view
        .files
        .iter()
        .map(|file| {
            let bytes: &[u8] = file.path.as_ref();
            bytes.to_vec()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            b".gitignore".to_vec(),
            b"payload.txt".to_vec(),
            b"tracked-link".to_vec(),
            b"untracked-link".to_vec()
        ]
    );
}

#[test]
fn refuses_separate_git_metadata() {
    // A separate Git directory keeps repository metadata outside the worktree's own `.git`.
    let parent = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let separate = parent.path().join("separate");
    git(
        parent.path(),
        &[
            "init",
            "--quiet",
            "--separate-git-dir",
            storage.path().to_str().unwrap(),
            separate.to_str().unwrap(),
        ],
    );
    std::fs::write(separate.join("file.txt"), b"separate\n").unwrap();
    git(&separate, &["add", "file.txt"]);
    assert_refused_before_sidecar("separate git dir", &separate, storage.path());
}

#[test]
fn refuses_linked_worktrees() {
    // A linked worktree is refused from both the linked checkout and its parent checkout.
    let main = repository();
    std::fs::write(main.path().join("file.txt"), b"tracked\n").unwrap();
    git(main.path(), &["add", "file.txt"]);
    commit(main.path(), "base");
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    git(
        main.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked-branch",
            linked.to_str().unwrap(),
        ],
    );
    assert_refused_before_sidecar(
        "linked worktree",
        &linked,
        &main.path().join(".git").join("worktrees").join("linked"),
    );
    assert_refused_before_sidecar(
        "parent of linked worktree",
        main.path(),
        &main.path().join(".git"),
    );
}

#[test]
fn refuses_object_alternates() {
    // An alternates object store borrows objects the recovery engine cannot vouch for.
    let directory = repository();
    let borrowed = tempfile::tempdir().unwrap();
    git(borrowed.path(), &["init", "--quiet"]);
    std::fs::write(directory.path().join("file.txt"), b"alternate\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    let objects = borrowed
        .path()
        .join(".git")
        .join("objects")
        .to_str()
        .unwrap()
        .to_owned();
    std::fs::write(
        directory
            .path()
            .join(".git")
            .join("objects")
            .join("info")
            .join("alternates"),
        format!("{objects}\n"),
    )
    .unwrap();
    assert_refused_before_sidecar(
        "alternates",
        directory.path(),
        &directory.path().join(".git"),
    );
}

#[test]
fn refuses_shallow_and_partially_cloned_repositories() {
    // A shallow clone borrows history the same way.
    let source = repository();
    std::fs::write(source.path().join("file.txt"), b"shallow\n").unwrap();
    git(source.path(), &["add", "file.txt"]);
    commit(source.path(), "base");
    let source_path = source.path().to_str().unwrap().to_owned();
    let source_url = format!("file://{source_path}");
    let shallow = tempfile::tempdir().unwrap();
    git(
        shallow.path(),
        &[
            "clone",
            "--quiet",
            "--depth",
            "1",
            source_url.as_str(),
            "shallow",
        ],
    );
    assert_refused_before_sidecar(
        "shallow clone",
        &shallow.path().join("shallow"),
        &shallow.path().join("shallow").join(".git"),
    );

    // A partial clone keeps its promise to fetch objects later.
    git(source.path(), &["config", "uploadpack.allowFilter", "true"]);
    let promisor = tempfile::tempdir().unwrap();
    git(
        promisor.path(),
        &[
            "clone",
            "--quiet",
            "--filter=blob:none",
            "--no-checkout",
            source_url.as_str(),
            "promisor",
        ],
    );
    assert_refused_before_sidecar(
        "promisor clone",
        &promisor.path().join("promisor"),
        &promisor.path().join("promisor").join(".git"),
    );
}

#[test]
fn refuses_reftable() {
    // A reftable reference store is a different on-disk format.
    let reftable = tempfile::tempdir().unwrap();
    let reftable_worktree = reftable.path().join("reftable");
    git(
        reftable.path(),
        &[
            "init",
            "--quiet",
            "--ref-format=reftable",
            reftable_worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(reftable_worktree.join("file.txt"), b"reftable\n").unwrap();
    git(&reftable_worktree, &["add", "file.txt"]);
    assert_refused_before_sidecar(
        "reftable",
        &reftable_worktree,
        &reftable_worktree.join(".git"),
    );
}

#[test]
fn refuses_split_index() {
    // A split index cannot be captured faithfully as raw bytes.
    let directory = repository();
    std::fs::write(directory.path().join("file.txt"), b"split\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    git(directory.path(), &["update-index", "--split-index"]);
    assert_refused_before_sidecar(
        "split index",
        directory.path(),
        &directory.path().join(".git"),
    );
}

#[test]
fn refuses_sparse_checkout_with_regular_index() {
    // A sparse checkout marks index entries the recovery engine cannot restore.
    let directory = repository();
    std::fs::create_dir_all(directory.path().join("kept")).unwrap();
    std::fs::create_dir_all(directory.path().join("pruned")).unwrap();
    std::fs::write(directory.path().join("kept").join("file.txt"), b"kept\n").unwrap();
    std::fs::write(
        directory.path().join("pruned").join("file.txt"),
        b"pruned\n",
    )
    .unwrap();
    git(directory.path(), &["add", "."]);
    commit(directory.path(), "base");
    git(
        directory.path(),
        &[
            "sparse-checkout",
            "set",
            "--cone",
            "--no-sparse-index",
            "kept",
        ],
    );
    assert_refused_before_sidecar(
        "sparse checkout",
        directory.path(),
        &directory.path().join(".git"),
    );
}

#[test]
fn refuses_nested_repository() {
    // A nested repository is not a capturable file tree.
    let directory = repository();
    git(directory.path(), &["init", "--quiet", "nested"]);
    std::fs::write(
        directory.path().join("nested").join("file.txt"),
        b"nested\n",
    )
    .unwrap();
    assert_refused_before_sidecar(
        "nested repository",
        directory.path(),
        &directory.path().join(".git"),
    );
}

#[test]
fn refuses_conflicted_index() {
    // An unresolved merge leaves conflicted index stages behind; removing MERGE_HEAD
    // isolates the index-conflict refusal from the in-progress marker refusal.
    let directory = repository();
    git(directory.path(), &["checkout", "--quiet", "-b", "base"]);
    std::fs::write(directory.path().join("file.txt"), b"base\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    commit(directory.path(), "base");
    git(directory.path(), &["checkout", "--quiet", "-b", "side"]);
    std::fs::write(directory.path().join("file.txt"), b"side\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    commit(directory.path(), "side");
    git(directory.path(), &["checkout", "--quiet", "base"]);
    std::fs::write(directory.path().join("file.txt"), b"mainline\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    commit(directory.path(), "mainline");
    git_expect_failure(directory.path(), &["merge", "side"]);
    std::fs::remove_file(directory.path().join(".git").join("MERGE_HEAD")).unwrap();
    assert_refused_before_sidecar(
        "conflicted index",
        directory.path(),
        &directory.path().join(".git"),
    );
}

#[test]
fn refuses_special_worktree_entry() {
    // A named pipe is a worktree entry with no faithful restore.
    let directory = repository();
    mkfifo(&directory.path().join("pipe"));
    assert_refused_before_sidecar(
        "special entry",
        directory.path(),
        &directory.path().join(".git"),
    );
}

#[test]
fn refuses_linked_parent_during_capture() {
    let directory = repository();
    std::fs::create_dir_all(directory.path().join("sub")).unwrap();
    std::fs::write(directory.path().join("sub").join("file.txt"), b"inside\n").unwrap();
    git(directory.path(), &["add", "sub/file.txt"]);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("file.txt"), b"outside the worktree\n").unwrap();
    std::fs::remove_dir_all(directory.path().join("sub")).unwrap();
    symlink(outside.path(), directory.path().join("sub")).unwrap();

    let repository = LocalRepository::discover(directory.path()).unwrap();
    let error = repository.adopt(AdoptOptions::default()).unwrap_err();
    assert!(matches!(error, Error::UnsupportedRepository(_)));
    assert!(!repository.git_dir().join("git2").exists());
}

#[test]
fn split_index_enabled_after_discovery_is_refused_before_sidecar() {
    let directory = repository();
    std::fs::write(directory.path().join("file.txt"), b"tracked\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    let repository = LocalRepository::discover(directory.path()).unwrap();
    git(directory.path(), &["update-index", "--split-index"]);

    let error = repository.adopt(AdoptOptions::default()).unwrap_err();
    assert!(
        matches!(error, Error::UnsupportedRepository(_)),
        "capture must revalidate index semantics, got {error:?}"
    );
    assert!(!repository.git_dir().join("git2").exists());
}

#[test]
fn repeated_unchanged_import_is_a_no_op() {
    let directory = repository();
    std::fs::write(directory.path().join("file.txt"), b"steady\n").unwrap();
    git(directory.path(), &["add", "file.txt"]);
    commit(directory.path(), "steady");

    let repo = LocalRepository::discover(directory.path()).unwrap();
    let adopt = repo.adopt(AdoptOptions::default()).unwrap();

    let first = repo.import_external_changes().unwrap();
    assert!(!first.changed);
    assert_eq!(first.operation_id, adopt.operation_id);
    assert_eq!(first.view_id, adopt.view_id);

    let second = repo.import_external_changes().unwrap();
    assert!(!second.changed);
    assert_eq!(second.operation_id, adopt.operation_id);
    assert_eq!(second.view_id, adopt.view_id);
}
