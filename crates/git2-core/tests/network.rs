#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use git2_core::commit;
use git2_core::log::{self, LogOptions};
use git2_core::network::{self, CloneOptions, FetchOptions, PullMode, PullOptions, PushOptions};
use git2_core::recovery::RecoveryStore;
use git2_core::remote::{self, RemoteAction};
use git2_core::repository::{AdoptOptions, LocalRepository};
use git2_core::stage;
use git2_core::transaction::{self, PendingPhase, RecoveryOutcome, RecoveryRepository};
use git2_core::undo;

fn git(directory: &Path, arguments: &[&str]) -> Output {
    let output = Command::new("git")
        .args(arguments)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Network Test")
        .env("GIT_AUTHOR_EMAIL", "net@example.invalid")
        .env("GIT_COMMITTER_NAME", "Network Test")
        .env("GIT_COMMITTER_EMAIL", "net@example.invalid")
        .output()
        .expect("Git must be installed for network tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed in {}:\nstdout:\n{}\nstderr:\n{}",
        directory.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn setup_remote_with_branches(root: &Path) -> std::path::PathBuf {
    let bare = root.join("upstream.git");
    let seed = root.join("seed");
    fs::create_dir_all(&seed).unwrap();
    git(
        root,
        &[
            "init",
            "--quiet",
            "--bare",
            "-b",
            "main",
            bare.to_str().unwrap(),
        ],
    );
    git(&seed, &["init", "--quiet", "-b", "main"]);
    git(&seed, &["config", "user.name", "Seed"]);
    git(&seed, &["config", "user.email", "seed@example.invalid"]);
    fs::write(seed.join("app.txt"), b"main v1\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "main v1"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);

    git(&seed, &["checkout", "--quiet", "-b", "feature"]);
    fs::write(seed.join("feat.txt"), b"feature v1\n").unwrap();
    git(&seed, &["add", "feat.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "feature v1"]);
    git(
        &seed,
        &["push", "--quiet", bare.to_str().unwrap(), "feature"],
    );
    bare
}

#[test]
fn clone_auto_adopts_supports_branch_origin_no_adopt_and_rejects_unsafe_urls() {
    let root = tempfile::tempdir().unwrap();
    let bare = setup_remote_with_branches(root.path());
    let bare_str = bare.to_str().unwrap().to_owned();

    // Derived directory + auto-adoption.
    let outcome = network::clone_repo(
        root.path(),
        &CloneOptions {
            url: bare_str.clone(),
            directory: None,
            branch: Some("feature".to_owned()),
            origin: Some("upstream".to_owned()),
            adopt: true,
            max_file_bytes: AdoptOptions::default().max_file_bytes,
        },
    )
    .unwrap();
    assert_eq!(outcome.workdir, root.path().join("upstream"));
    assert!(outcome.adoption.is_some());
    assert!(RecoveryStore::exists(outcome.workdir.join(".git")).unwrap());
    let cloned_repo = LocalRepository::discover(&outcome.workdir).unwrap();
    let entries = log::walk(&cloned_repo, LogOptions::default()).unwrap();
    assert_eq!(entries[0].commit.message.as_slice(), b"feature v1\n");
    let remotes = remote::run(&cloned_repo, &RemoteAction::List { verbose: false }).unwrap();
    assert_eq!(
        String::from_utf8(remotes.stdout).unwrap().trim(),
        "upstream"
    );
    git(&outcome.workdir, &["fsck", "--full", "--strict"]);

    // --no-adopt skips .git/git2 creation.
    let raw_clone = network::clone_repo(
        root.path(),
        &CloneOptions {
            url: bare_str,
            directory: Some(root.path().join("plain-clone")),
            branch: None,
            origin: None,
            adopt: false,
            max_file_bytes: AdoptOptions::default().max_file_bytes,
        },
    )
    .unwrap();
    assert!(raw_clone.adoption.is_none());
    assert!(!raw_clone.workdir.join(".git/git2").exists());

    // Refuse unsafe flags / ext:: protocol before invoking git.
    assert!(
        network::clone_repo(
            root.path(),
            &CloneOptions {
                url: "--upload-pack=evil".to_owned(),
                ..CloneOptions::default()
            }
        )
        .is_err()
    );
    assert!(
        network::clone_repo(
            root.path(),
            &CloneOptions {
                url: "ext::sh -c true".to_owned(),
                ..CloneOptions::default()
            }
        )
        .is_err()
    );
}

#[test]
fn remote_subcommands_manage_remotes_and_track_ref_removals() {
    let root = tempfile::tempdir().unwrap();
    let bare = setup_remote_with_branches(root.path());
    let bare2 = root.path().join("other.git");
    git(
        root.path(),
        &["init", "--quiet", "--bare", bare2.to_str().unwrap()],
    );

    let clone = network::clone_repo(
        root.path(),
        &CloneOptions {
            url: bare.to_str().unwrap().to_owned(),
            directory: Some(root.path().join("local")),
            ..CloneOptions::default()
        },
    )
    .unwrap();
    let repo = LocalRepository::discover(&clone.workdir).unwrap();

    remote::run(
        &repo,
        &RemoteAction::Add {
            name: "mirror".to_owned(),
            url: bare2.to_str().unwrap().to_owned(),
        },
    )
    .unwrap();
    let got = remote::run(
        &repo,
        &RemoteAction::GetUrl {
            name: "mirror".to_owned(),
            push: false,
            all: false,
        },
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(got.stdout).unwrap().trim(),
        bare2.to_str().unwrap()
    );

    remote::run(
        &repo,
        &RemoteAction::SetUrl {
            name: "mirror".to_owned(),
            new_url: bare.to_str().unwrap().to_owned(),
            old_url: None,
            push: false,
        },
    )
    .unwrap();

    remote::run(
        &repo,
        &RemoteAction::Rename {
            old: "origin".to_owned(),
            new: "upstream".to_owned(),
        },
    )
    .unwrap();
    assert!(
        clone.workdir.join(".git/refs/remotes/upstream").exists() || {
            let refs = String::from_utf8(git(&clone.workdir, &["for-each-ref"]).stdout).unwrap();
            refs.contains("refs/remotes/upstream/")
        }
    );

    remote::run(
        &repo,
        &RemoteAction::Remove {
            name: "upstream".to_owned(),
        },
    )
    .unwrap();
    let refs_after_remove =
        String::from_utf8(git(&clone.workdir, &["for-each-ref"]).stdout).unwrap();
    assert!(!refs_after_remove.contains("refs/remotes/upstream/"));

    // Undo restores the removed tracking refs cleanly.
    undo::apply(&repo).unwrap();
    let refs_after_undo = String::from_utf8(git(&clone.workdir, &["for-each-ref"]).stdout).unwrap();
    assert!(refs_after_undo.contains("refs/remotes/upstream/"));
}

#[test]
fn fetch_is_undoable_handles_prune_tags_noop_and_offline_failure() {
    let root = tempfile::tempdir().unwrap();
    let bare = setup_remote_with_branches(root.path());
    let clone = network::clone_repo(
        root.path(),
        &CloneOptions {
            url: bare.to_str().unwrap().to_owned(),
            directory: Some(root.path().join("local")),
            ..CloneOptions::default()
        },
    )
    .unwrap();
    let repo = LocalRepository::discover(&clone.workdir).unwrap();

    // Already up-to-date fetch appends no operation.
    let noop = network::fetch(&repo, &FetchOptions::default()).unwrap();
    assert!(noop.operation_id.is_none());

    // Push a new commit + tag + delete `feature` upstream from seed.
    let seed = root.path().join("seed");
    git(&seed, &["checkout", "--quiet", "main"]);
    fs::write(seed.join("app.txt"), b"main v2\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "main v2"]);
    git(&seed, &["tag", "v0.2"]);
    git(
        &seed,
        &["push", "--quiet", bare.to_str().unwrap(), "main", "v0.2"],
    );
    git(
        &seed,
        &[
            "push",
            "--quiet",
            bare.to_str().unwrap(),
            "--delete",
            "feature",
        ],
    );

    let fetched = network::fetch(
        &repo,
        &FetchOptions {
            remote: Some("origin".to_owned()),
            prune: true,
            tags: true,
            ..FetchOptions::default()
        },
    )
    .unwrap();
    assert!(fetched.operation_id.is_some());
    let refs = String::from_utf8(git(&clone.workdir, &["for-each-ref"]).stdout).unwrap();
    assert!(refs.contains("refs/tags/v0.2"));
    assert!(!refs.contains("refs/remotes/origin/feature"));

    // Undo reverts fetched tracking refs and tags; redo restores them cleanly.
    undo::apply(&repo).unwrap();
    let refs_undone = String::from_utf8(git(&clone.workdir, &["for-each-ref"]).stdout).unwrap();
    assert!(!refs_undone.contains("refs/tags/v0.2"));
    assert!(refs_undone.contains("refs/remotes/origin/feature"));

    undo::apply(&repo).unwrap();
    let refs_redone = String::from_utf8(git(&clone.workdir, &["for-each-ref"]).stdout).unwrap();
    assert!(refs_redone.contains("refs/tags/v0.2"));
    assert!(!refs_redone.contains("refs/remotes/origin/feature"));

    // Fetching from a missing remote fails cleanly without leaving pending/delegated stuck.
    assert!(
        network::fetch(
            &repo,
            &FetchOptions {
                remote: Some("/nonexistent/git2-missing-remote.git".to_owned()),
                ..FetchOptions::default()
            }
        )
        .is_err()
    );
    assert!(
        !RecoveryStore::open(repo.git_dir())
            .unwrap()
            .has_pending()
            .unwrap()
    );
    git(&clone.workdir, &["fsck", "--full", "--strict"]);
}

#[test]
fn pull_ff_merge_rebase_and_conflict_rollback_or_recover_work() {
    let root = tempfile::tempdir().unwrap();
    let bare = setup_remote_with_branches(root.path());
    let seed = root.path().join("seed");

    let clone = network::clone_repo(
        root.path(),
        &CloneOptions {
            url: bare.to_str().unwrap().to_owned(),
            directory: Some(root.path().join("local")),
            ..CloneOptions::default()
        },
    )
    .unwrap();
    git(&clone.workdir, &["config", "user.name", "Local"]);
    git(
        &clone.workdir,
        &["config", "user.email", "local@example.invalid"],
    );
    let repo = LocalRepository::discover(&clone.workdir).unwrap();

    // 1. Upstream advances main -> default --ff-only pull succeeds & is undoable.
    git(&seed, &["checkout", "--quiet", "main"]);
    fs::write(seed.join("app.txt"), b"upstream v2\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "upstream v2"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);

    let ff = network::pull(&repo, &PullOptions::default()).unwrap();
    assert!(ff.operation_id.is_some());
    assert_eq!(
        fs::read(clone.workdir.join("app.txt")).unwrap(),
        b"upstream v2\n"
    );
    undo::apply(&repo).unwrap();
    assert_eq!(
        fs::read(clone.workdir.join("app.txt")).unwrap(),
        b"main v1\n"
    );
    network::pull(&repo, &PullOptions::default()).unwrap();

    // 2. Diverge local & upstream on DIFFERENT files -> ff-only refuses cleanly (no pending), --merge succeeds!
    fs::write(seed.join("remote_only.txt"), b"from upstream\n").unwrap();
    git(&seed, &["add", "remote_only.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "remote only"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);

    fs::write(clone.workdir.join("local_only.txt"), b"from local\n").unwrap();
    stage::apply(&repo, &clone.workdir, true, &[]).unwrap();
    commit::apply(&repo, "local only").unwrap();

    assert!(network::pull(&repo, &PullOptions::default()).is_err());
    assert!(
        !RecoveryStore::open(repo.git_dir())
            .unwrap()
            .has_pending()
            .unwrap()
    );

    let merged = network::pull(
        &repo,
        &PullOptions {
            mode: PullMode::Merge,
            ..PullOptions::default()
        },
    )
    .unwrap();
    assert!(merged.operation_id.is_some());
    assert!(clone.workdir.join("remote_only.txt").exists());
    undo::apply(&repo).unwrap();
    assert!(!clone.workdir.join("remote_only.txt").exists());

    // Rebase pull on clean divergence works and is undoable too.
    let rebased = network::pull(
        &repo,
        &PullOptions {
            mode: PullMode::Rebase,
            ..PullOptions::default()
        },
    )
    .unwrap();
    assert!(rebased.operation_id.is_some());
    assert!(clone.workdir.join("remote_only.txt").exists());
    assert!(clone.workdir.join("local_only.txt").exists());
    undo::apply(&repo).unwrap();
    assert!(!clone.workdir.join("remote_only.txt").exists());
    git(&clone.workdir, &["fsck", "--full", "--strict"]);
}

fn setup_conflict_clone(root: &Path) -> (std::path::PathBuf, git2_core::object::GitOid) {
    let bare = setup_remote_with_branches(root);
    let seed = root.join("seed");
    let clone = network::clone_repo(
        root,
        &CloneOptions {
            url: bare.to_str().unwrap().to_owned(),
            directory: Some(root.join("local")),
            ..CloneOptions::default()
        },
    )
    .unwrap();
    git(&clone.workdir, &["config", "user.name", "Local"]);
    git(
        &clone.workdir,
        &["config", "user.email", "local@example.invalid"],
    );
    let repo = LocalRepository::discover(&clone.workdir).unwrap();

    git(&seed, &["checkout", "--quiet", "main"]);
    fs::write(seed.join("app.txt"), b"upstream conflict edit\n").unwrap();
    git(&seed, &["add", "app.txt"]);
    git(&seed, &["commit", "--quiet", "-m", "upstream conflict"]);
    git(&seed, &["push", "--quiet", bare.to_str().unwrap(), "main"]);

    fs::write(clone.workdir.join("app.txt"), b"local conflict edit\n").unwrap();
    stage::apply(&repo, &clone.workdir, true, &[]).unwrap();
    let pre_conflict = commit::apply(&repo, "local conflict").unwrap();
    (clone.workdir, pre_conflict.commit_oid)
}

#[test]
fn pull_conflict_rollback_and_manual_recover_work() {
    let root = tempfile::tempdir().unwrap();
    let (workdir, pre_conflict_oid) = setup_conflict_clone(root.path());
    let repo = LocalRepository::discover(&workdir).unwrap();
    let rec_repo = RecoveryRepository::open(&workdir).unwrap();

    // 1. Conflicted --merge -> rollback cleanly restores exact pre-pull state.
    let merge_opts = PullOptions {
        mode: PullMode::Merge,
        ..PullOptions::default()
    };
    assert!(network::pull(&repo, &merge_opts).is_err());
    let pending = transaction::pending(&rec_repo).unwrap().unwrap();
    assert_eq!(pending.phase, PendingPhase::Delegated);
    assert!(pending.conflict_state);
    assert!(transaction::recover(&rec_repo).is_err());

    assert!(matches!(
        transaction::rollback(&rec_repo).unwrap(),
        RecoveryOutcome::RolledBack { .. }
    ));
    assert!(!workdir.join(".git/MERGE_HEAD").exists());
    assert_eq!(
        fs::read(workdir.join("app.txt")).unwrap(),
        b"local conflict edit\n"
    );
    assert_eq!(
        log::walk(
            &LocalRepository::discover(&workdir).unwrap(),
            LogOptions::default()
        )
        .unwrap()[0]
            .oid,
        pre_conflict_oid
    );

    // 2. Conflicted --rebase -> rollback removes rebase-merge/ and restores branch cleanly.
    let rebase_opts = PullOptions {
        mode: PullMode::Rebase,
        ..PullOptions::default()
    };
    assert!(network::pull(&repo, &rebase_opts).is_err());
    assert!(
        transaction::pending(&rec_repo)
            .unwrap()
            .unwrap()
            .conflict_state
    );
    assert!(matches!(
        transaction::rollback(&rec_repo).unwrap(),
        RecoveryOutcome::RolledBack { .. }
    ));
    assert!(!workdir.join(".git/rebase-merge").exists());
    assert_eq!(
        fs::read(workdir.join("app.txt")).unwrap(),
        b"local conflict edit\n"
    );

    // 3. Conflicted --merge resolved manually in Git -> recover finalizes undoable operation.
    assert!(network::pull(&repo, &merge_opts).is_err());
    fs::write(workdir.join("app.txt"), b"resolved cleanly\n").unwrap();
    git(&workdir, &["add", "app.txt"]);
    git(&workdir, &["commit", "--quiet", "--no-edit"]);
    assert!(matches!(
        transaction::recover(&rec_repo).unwrap(),
        RecoveryOutcome::Finalized { .. }
    ));
    assert_eq!(
        fs::read(workdir.join("app.txt")).unwrap(),
        b"resolved cleanly\n"
    );

    undo::apply(&LocalRepository::discover(&workdir).unwrap()).unwrap();
    assert_eq!(
        fs::read(workdir.join("app.txt")).unwrap(),
        b"local conflict edit\n"
    );
    git(&workdir, &["fsck", "--full", "--strict"]);
}

#[test]
fn push_refuses_undo_refuses_private_refs_and_keeps_sidecar_and_staged_secret_private() {
    let root = tempfile::tempdir().unwrap();
    let bare = setup_remote_with_branches(root.path());
    let clone = network::clone_repo(
        root.path(),
        &CloneOptions {
            url: bare.to_str().unwrap().to_owned(),
            directory: Some(root.path().join("local")),
            ..CloneOptions::default()
        },
    )
    .unwrap();
    git(&clone.workdir, &["config", "user.name", "Pusher"]);
    git(
        &clone.workdir,
        &["config", "user.email", "pusher@example.invalid"],
    );
    let repo = LocalRepository::discover(&clone.workdir).unwrap();

    // Private refs/git2/* refspec refused before running git.
    assert!(
        network::push(
            &repo,
            &PushOptions {
                remote: Some("origin".to_owned()),
                refspecs: vec!["HEAD:refs/git2/bad".to_owned()],
                ..PushOptions::default()
            }
        )
        .is_err()
    );

    // Stage a secret blob into local recovery history, then discard it before committing.
    fs::write(
        clone.workdir.join("secret.txt"),
        b"PRIVATE_STAGED_ONLY_SECRET\n",
    )
    .unwrap();
    let secret_oid =
        String::from_utf8(git(&clone.workdir, &["hash-object", "secret.txt"]).stdout).unwrap();
    let secret_oid = secret_oid.trim().to_owned();
    stage::apply(&repo, &clone.workdir, true, &[]).unwrap();
    git(&clone.workdir, &["reset", "--quiet", "--hard", "HEAD"]);

    fs::write(clone.workdir.join("app.txt"), b"pushed commit\n").unwrap();
    stage::apply(&repo, &clone.workdir, true, &[]).unwrap();
    let pushed_commit = commit::apply(&repo, "push this commit").unwrap();

    // --dry-run succeeds without touching remote.
    let dry = network::push(
        &repo,
        &PushOptions {
            remote: Some("origin".to_owned()),
            dry_run: true,
            ..PushOptions::default()
        },
    )
    .unwrap();
    assert!(dry.operation_id.is_none());

    let pushed = network::push(
        &repo,
        &PushOptions {
            remote: Some("origin".to_owned()),
            refspecs: vec!["main".to_owned()],
            set_upstream: true,
            ..PushOptions::default()
        },
    )
    .unwrap();
    assert!(pushed.operation_id.is_some());

    // git2 undo MUST refuse to undo a git2:push!
    let undo_err = undo::apply(&repo).unwrap_err();
    assert!(
        undo_err.to_string().contains("remote push"),
        "expected remote push undo refusal, got: {undo_err}"
    );

    // Verify bare remote has our commit, NO sidecar/refs/git2, and NO secret staged blob.
    let remote_head =
        String::from_utf8(git(&bare, &["rev-parse", "refs/heads/main"]).stdout).unwrap();
    assert_eq!(remote_head.trim(), pushed_commit.commit_oid.to_hex());
    assert!(!bare.join("git2").exists());
    let remote_refs = String::from_utf8(git(&bare, &["for-each-ref"]).stdout).unwrap();
    assert!(!remote_refs.contains("refs/git2"));

    let secret_check = Command::new("git")
        .args(["cat-file", "-e", &secret_oid])
        .current_dir(&bare)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(!secret_check.status.success());
    git(&bare, &["fsck", "--full", "--strict"]);
}
