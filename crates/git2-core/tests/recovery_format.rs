#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use git2_core::Error;
use git2_core::object::GitOid;
use git2_core::recovery::{
    CapturePolicy, ContentId, Operation, OperationKind, RecoveryStore, RetainedObject, View,
};

const LEGACY_FORMAT: &[u8] = include_bytes!("fixtures/recovery/format-v1.txt");

fn initialize(git_dir: &Path) -> RecoveryStore {
    RecoveryStore::initialize(git_dir, CapturePolicy::default()).unwrap()
}

fn record_path(store: &RecoveryStore, namespace: &str, id: ContentId) -> PathBuf {
    let hex = id.to_hex();
    store.root().join(namespace).join(&hex[..2]).join(&hex[2..])
}

fn sample_view() -> View {
    View {
        head: b"H\n".to_vec().into(),
        refs: Vec::new(),
        index: None,
        files: Vec::new(),
        retained_objects: Vec::new(),
    }
}

#[test]
fn v1_is_refused_without_a_byte_changed() {
    let git_dir = tempfile::tempdir().unwrap();
    let root = git_dir.path().join("git2");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    for (name, bytes) in [("format", LEGACY_FORMAT), ("HEAD", b"sentinel old history")] {
        let path = root.join(name);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    let Err(Error::UnsupportedRepository(message)) = RecoveryStore::open(git_dir.path()) else {
        panic!("a version 1 store must be refused");
    };
    assert!(message.contains('1'), "{message}");
    assert!(RecoveryStore::initialize(git_dir.path(), CapturePolicy::default()).is_err());
    assert_eq!(fs::read(root.join("format")).unwrap(), LEGACY_FORMAT);
    assert_eq!(
        fs::read(root.join("HEAD")).unwrap(),
        b"sentinel old history"
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
}

#[test]
fn v2_view_matches_the_golden_bytes_and_rejects_truncation() {
    let hex: String = include_str!("fixtures/recovery/view-v2.hex")
        .split_ascii_whitespace()
        .collect();
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    let golden: Vec<_> = pairs
        .iter()
        .map(|pair| u8::from_str_radix(str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let mut view = sample_view();
    view.retained_objects.push(RetainedObject {
        oid: GitOid::from_bytes([0xaa; 20]),
        content: ContentId::from_hex(&[b'b'; 64]).unwrap(),
    });
    assert_eq!(view.encode().unwrap(), golden);
    assert_eq!(View::decode(&golden).unwrap(), view);
    for end in 0..golden.len() {
        assert!(View::decode(&golden[..end]).is_err(), "truncation at {end}");
    }
    let mut trailing = golden;
    trailing.push(0);
    assert!(View::decode(&trailing).is_err());
}

#[test]
fn rejects_duplicate_retained_ids_on_encode() {
    let mut view = sample_view();
    let oid = GitOid::from_bytes([0xaa; 20]);
    view.retained_objects = vec![
        RetainedObject {
            oid,
            content: ContentId::for_bytes(b"first"),
        },
        RetainedObject {
            oid,
            content: ContentId::for_bytes(b"second"),
        },
    ];
    assert!(view.encode().is_err());
}

#[test]
fn refuses_overlimit_view_fields_and_counts() {
    let mut view = sample_view();
    view.head = vec![0; 16 * 1024 * 1024 + 1].into();
    assert!(view.encode().is_err());
    let mut tiny = b"git2-view\0\x02".to_vec();
    tiny.extend_from_slice(&0_u64.to_le_bytes()); // HEAD
    tiny.extend_from_slice(&10_000_000_u64.to_le_bytes()); // refs, with no records
    assert!(View::decode(&tiny).is_err());
}

#[test]
fn store_directories_and_files_are_owner_private() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let id = store.put_content(b"owner-private data").unwrap();
    let record = record_path(&store, "cas", id);
    let root = store.root();
    let fanout = record.parent().unwrap();
    for path in [root, &root.join("cas"), fanout] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
    for path in [root.join("format"), root.join("policy"), record] {
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
}

#[test]
fn unsafe_store_directory_is_refused() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    fs::set_permissions(store.root(), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(RecoveryStore::open(git_dir.path()).is_err());
}

#[test]
fn symlink_store_is_refused() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let store = initialize(first.path());
    symlink(store.root(), second.path().join("git2")).unwrap();
    assert!(RecoveryStore::exists(second.path()).unwrap());
    assert!(RecoveryStore::open(second.path()).is_err());
}

#[test]
fn unsafe_record_mode_is_refused() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let id = store.put_content(b"data").unwrap();
    let path = record_path(&store, "cas", id);
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.read_content(id).is_err());
}

#[test]
fn immutable_conflict_never_replaces_existing_record() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let id = store.put_content(b"expected").unwrap();
    let path = record_path(&store, "cas", id);
    fs::write(&path, b"different").unwrap();
    assert!(matches!(
        store.put_content(b"expected"),
        Err(Error::CorruptRecovery(_))
    ));
    assert_eq!(fs::read(path).unwrap(), b"different");
    assert!(matches!(
        store.read_content(id),
        Err(Error::CorruptRecovery(_))
    ));
}

#[test]
fn oversized_operation_cannot_be_published() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let id = ContentId::for_bytes(b"view");
    let operation = Operation {
        parent: None,
        before: id,
        after: id,
        kind: OperationKind::Git2(vec![b'x'; 1024 * 1024].into()),
        timestamp_seconds: 0,
    };
    assert!(store.put_operation(&operation).is_err());
    assert_eq!(
        fs::read_dir(store.root().join("operations"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn oversized_view_file_is_refused() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let id = store.put_view(&sample_view()).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(record_path(&store, "views", id))
        .unwrap()
        .set_len(64 * 1024 * 1024 + 1)
        .unwrap();
    assert!(matches!(
        store.read_view(id),
        Err(Error::CorruptRecovery(_))
    ));
}

#[test]
fn missing_view_prevents_head_publication() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let missing = ContentId::for_bytes(b"missing view");
    let operation_id = store
        .put_operation(&Operation {
            parent: None,
            before: missing,
            after: missing,
            kind: OperationKind::Adopt,
            timestamp_seconds: 0,
        })
        .unwrap();
    assert!(store.set_head(operation_id).is_err());
    assert!(!store.root().join("HEAD").exists());
}

#[test]
fn damaged_view_cannot_be_selected() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let view_id = store.put_view(&sample_view()).unwrap();
    let operation_id = store
        .put_operation(&Operation {
            parent: None,
            before: view_id,
            after: view_id,
            kind: OperationKind::Adopt,
            timestamp_seconds: 0,
        })
        .unwrap();
    fs::write(record_path(&store, "views", view_id), b"damaged bytes").unwrap();
    assert!(matches!(
        store.set_head(operation_id),
        Err(Error::CorruptRecovery(_))
    ));
    assert!(!store.root().join("HEAD").exists());
}

#[test]
fn missing_parent_prevents_head_publication() {
    let git_dir = tempfile::tempdir().unwrap();
    let store = initialize(git_dir.path());
    let view = store.put_view(&sample_view()).unwrap();
    let operation_id = store
        .put_operation(&Operation {
            parent: Some(ContentId::for_bytes(b"missing parent")),
            before: view,
            after: view,
            kind: OperationKind::ExternalGit,
            timestamp_seconds: 0,
        })
        .unwrap();
    assert!(store.set_head(operation_id).is_err());
    assert!(!store.root().join("HEAD").exists());
}

#[test]
fn incomplete_store_has_preservation_instructions() {
    let git_dir = tempfile::tempdir().unwrap();
    let root = git_dir.path().join("git2");
    fs::create_dir(&root).unwrap();
    let error = RecoveryStore::open(git_dir.path()).unwrap_err().to_string();
    assert!(
        error.contains("incomplete") && error.contains("Archive"),
        "{error}"
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

    let initialized = tempfile::tempdir().unwrap();
    let store = initialize(initialized.path());
    let error = store.head().unwrap_err().to_string();
    assert!(
        error.contains("incomplete") && error.contains("Archive"),
        "{error}"
    );
    assert!(!store.root().join("HEAD").exists());
}
