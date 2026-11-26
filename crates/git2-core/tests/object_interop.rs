use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use git2_core::object::{GitOid, ObjectKind, RawObject};
use git2_core::store::ObjectStore;
use tempfile::TempDir;

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
        .expect("Git must be installed for differential tests");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_with_stdin(directory: &Path, arguments: &[&str], input: &[u8]) -> Output {
    let mut child = git_command(directory, arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Git must be installed for differential tests");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input)
        .expect("write Git stdin");
    let output = child.wait_with_output().expect("wait for Git");
    assert!(
        output.status.success(),
        "git {arguments:?} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn repository() -> TempDir {
    let directory = tempfile::tempdir().expect("temporary repository");
    git(directory.path(), &["init", "--quiet"]);
    directory
}

#[test]
fn loose_blob_written_by_git2_is_readable_by_git() {
    let directory = repository();
    let store = ObjectStore::discover(directory.path()).expect("discover repository");
    let bytes = b"binary\0payload\n\xff".to_vec();
    let object = RawObject {
        kind: ObjectKind::Blob,
        data: bytes.clone(),
    };

    let git2_oid = store.write(&object).expect("write loose object");
    let git_output = git_with_stdin(directory.path(), &["hash-object", "--stdin"], &bytes);
    assert_eq!(
        git2_oid.to_hex().as_bytes(),
        git_output.stdout.strip_suffix(b"\n").expect("newline")
    );

    let output = git(directory.path(), &["cat-file", "blob", &git2_oid.to_hex()]);
    assert_eq!(output.stdout, bytes);
}

#[test]
fn loose_blob_written_by_git_is_readable_by_git2() {
    let directory = repository();
    let bytes = b"from Git\0without UTF-8: \xfe";
    let output = git_with_stdin(directory.path(), &["hash-object", "-w", "--stdin"], bytes);
    let oid = String::from_utf8(output.stdout)
        .expect("Git oid is UTF-8")
        .trim()
        .parse::<GitOid>()
        .expect("Git produced a full SHA-1");

    let object = ObjectStore::discover(directory.path())
        .expect("discover repository")
        .read(oid)
        .expect("read Git loose object");
    assert_eq!(object.kind, ObjectKind::Blob);
    assert_eq!(object.data, bytes);
}

#[test]
fn packed_blob_is_read_through_gix() {
    let directory = repository();
    std::fs::write(directory.path().join("packed.txt"), b"eventually packed\n")
        .expect("write file");
    git(directory.path(), &["add", "packed.txt"]);
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
    let output = git(directory.path(), &["rev-parse", "HEAD:packed.txt"]);
    let oid = String::from_utf8(output.stdout)
        .expect("Git oid is UTF-8")
        .trim()
        .parse::<GitOid>()
        .expect("Git produced a full SHA-1");
    git(directory.path(), &["gc", "--quiet", "--prune=now"]);

    let store = ObjectStore::discover(directory.path()).expect("discover repository");
    let hex = oid.to_hex();
    assert!(
        !store
            .git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
            .exists(),
        "fixture should be packed"
    );
    let object = store.read(oid).expect("read packed object");
    assert_eq!(object.kind, ObjectKind::Blob);
    assert_eq!(object.data, b"eventually packed\n");
}

#[cfg(unix)]
#[test]
fn write_refuses_corrupt_or_symlink_loose_object_destination() {
    let directory = repository();
    let store = ObjectStore::discover(directory.path()).expect("discover repository");
    let object = RawObject {
        kind: ObjectKind::Blob,
        data: b"verified loose payload\n".to_vec(),
    };
    let id = object.id();
    let hex = id.to_hex();
    let parent = store.git_dir().join("objects").join(&hex[..2]);
    std::fs::create_dir_all(&parent).unwrap();
    let target = parent.join(&hex[2..]);

    // Pre-existing corrupt regular file must fail verification instead of silently returning Ok.
    std::fs::write(&target, b"corrupt loose bytes").unwrap();
    assert!(store.write(&object).is_err());
    std::fs::remove_file(&target).unwrap();

    // Pre-existing symlink at loose object path must be refused.
    let decoy = directory.path().join("decoy");
    std::fs::write(&decoy, b"decoy").unwrap();
    std::os::unix::fs::symlink(&decoy, &target).unwrap();
    assert!(store.read(id).is_err());
    assert!(store.write(&object).is_err());
}
