//! Capture and verify the Git object closure of a recovery view.

use std::collections::{BTreeMap, btree_map::Entry};

use bstr::{BStr, BString, ByteSlice};

use crate::object::{GitOid, ObjectKind, canonical_object_parts, hash_object};
use crate::recovery::{ContentId, RecoveryStore, RetainedObject, View};
use crate::repository::{decode_index, validate_index};
use crate::store::ObjectStore;
use crate::{Error, Result};

const HASH: gix::hash::Kind = gix::hash::Kind::Sha1;
pub(crate) const MAX_OBJECT_BYTES: usize = 128 * 1024 * 1024;
const MAX_OBJECTS: usize = 1_000_000;
const MAX_SYMBOLIC_HOPS: usize = 32;

#[derive(Clone, Copy)]
pub(crate) struct Root(GitOid, Option<ObjectKind>);

/// Retain canonical bytes for every object reachable from `roots`.
///
/// # Errors
///
/// Returns an error if the graph is incomplete, unsupported, corrupt, or over the retention limits.
pub fn retain(
    store: &RecoveryStore,
    objects: &ObjectStore,
    roots: &[GitOid],
) -> Result<Vec<RetainedObject>> {
    retain_roots(
        store,
        objects,
        roots.iter().copied().map(|id| Root(id, None)),
    )
}

pub(crate) fn retain_roots(
    store: &RecoveryStore,
    objects: &ObjectStore,
    roots: impl IntoIterator<Item = Root>,
) -> Result<Vec<RetainedObject>> {
    let mut reader = objects.bounded_reader(MAX_OBJECT_BYTES)?;
    walk(roots, |id, graph| {
        let object = reader.read(id)?;
        graph.visit(id, object.kind, &object.data)?;
        let content = store.put_content(&object.canonical_bytes())?;
        Ok((object.kind, content))
    })
}

/// Verify that a view retains exactly its required Git object graph, using only the sidecar.
///
/// # Errors
///
/// Returns an error for missing, invalid, unexpected, or incorrectly typed objects or roots.
pub fn verify(store: &RecoveryStore, view: &View) -> Result<()> {
    let index = view.index.map(|id| store.read_content(id)).transpose()?;
    let index = index.as_deref().map(decode_index).transpose()?;
    let roots = roots(&view.head, &view.refs, index.as_ref())?;
    let retained = walk(roots, |id, graph| {
        let position = view
            .retained_objects
            .binary_search_by_key(&id, |entry| entry.oid)
            .map_err(|_| Error::CorruptRecovery(format!("missing retained Git object {id}")))?;
        let content = view.retained_objects[position].content;
        let canonical = store.read_content_bounded(content, MAX_OBJECT_BYTES + 32)?;
        let (kind, data) = canonical_object_parts(&canonical)?;
        if data.len() > MAX_OBJECT_BYTES || hash_object(kind, data) != id {
            return Err(Error::CorruptRecovery(format!(
                "retained Git object {id} has the wrong size or digest"
            )));
        }
        graph.visit(id, kind, data)?;
        Ok((kind, content))
    })?;
    if retained != view.retained_objects {
        return Err(Error::CorruptRecovery(
            "view contains unexpected or unordered retained Git objects".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn roots(
    head: &[u8],
    refs: &[(BString, BString)],
    index: Option<&gix::index::State>,
) -> Result<Vec<Root>> {
    let mut roots = Graph::default();
    let mut packed = BTreeMap::new();
    let mut loose = BTreeMap::new();
    let mut has_packed_refs = false;
    for (name, bytes) in refs {
        if name.as_slice() == b"packed-refs" {
            if std::mem::replace(&mut has_packed_refs, true) {
                return Err(metadata_error("duplicate packed-refs file"));
            }
            for reference in
                gix::refs::packed::Iter::new(bytes.as_ref(), HASH).map_err(metadata_error)?
            {
                let reference = reference.map_err(metadata_error)?;
                let name = reference.name.as_bstr();
                check_ref_name(name)?;
                let oid = GitOid::from_hex(reference.target.as_ref())?;
                if packed.insert(name, oid).is_some() {
                    return Err(metadata_error("duplicate packed reference"));
                }
                roots.require(Root(oid, None))?;
                if let Some(peeled) = reference.object {
                    roots.require(Root(GitOid::from_hex(peeled.as_ref())?, None))?;
                }
            }
        } else {
            let name = name.as_bstr();
            check_ref_name(name)?;
            let target = ref_target(bytes.as_ref())?;
            if loose.insert(name, target).is_some() {
                return Err(metadata_error("duplicate loose reference"));
            }
            if let Target::Direct(oid) = target {
                roots.require(Root(oid, None))?;
            }
        }
    }
    for (name, target) in &loose {
        if resolve(*target, &loose, &packed)?.is_none() {
            return Err(metadata_error(format!("symbolic ref {name:?} is dangling")));
        }
    }
    match resolve(ref_target(head)?, &loose, &packed)? {
        Some(oid) => roots.require(Root(oid, Some(ObjectKind::Commit)))?,
        None if head.starts_with(b"ref: refs/heads/") => {}
        None => {
            return Err(metadata_error(
                "HEAD does not name an object or an unborn branch",
            ));
        }
    }
    if let Some(index) = index {
        index_roots(index, &mut roots)?;
    }
    Ok(roots
        .objects
        .into_iter()
        .map(|(id, node)| Root(id, node.kind))
        .collect())
}

fn index_roots(index: &gix::index::State, roots: &mut Graph) -> Result<()> {
    validate_index(index)?;
    if index.resolve_undo().is_some() {
        return Err(Error::UnsupportedRepository(
            "indexes with resolve-undo (REUC) records are not supported".to_owned(),
        ));
    }
    for entry in index.entries() {
        if entry
            .flags
            .contains(gix::index::entry::Flags::INTENT_TO_ADD)
        {
            if entry.id.is_null() || entry.id == gix::hash::ObjectId::empty_blob(HASH) {
                continue;
            }
            return Err(metadata_error(
                "intent-to-add entry has a non-placeholder object ID",
            ));
        }
        if !matches!(
            entry.mode,
            gix::index::entry::Mode::FILE
                | gix::index::entry::Mode::FILE_EXECUTABLE
                | gix::index::entry::Mode::SYMLINK
        ) {
            return Err(metadata_error("unsupported staged entry mode"));
        }
        roots.require(Root(raw_oid(entry.id.as_slice())?, Some(ObjectKind::Blob)))?;
    }
    let mut cache: Vec<_> = index.tree().into_iter().collect();
    while let Some(tree) = cache.pop() {
        if tree.num_entries.is_some() {
            roots.require(Root(raw_oid(tree.id.as_slice())?, Some(ObjectKind::Tree)))?;
        }
        cache.extend(&tree.children);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Target<'a> {
    Direct(GitOid),
    Symbolic(&'a BStr),
}

fn ref_target(bytes: &[u8]) -> Result<Target<'_>> {
    let bytes = bytes.trim_end_with(|byte| byte.is_ascii_whitespace());
    if let Some(name) = bytes.strip_prefix(b"ref: ") {
        let name = name.as_bstr();
        check_ref_name(name)?;
        Ok(Target::Symbolic(name))
    } else {
        Ok(Target::Direct(GitOid::from_hex(bytes)?))
    }
}

fn check_ref_name(name: &BStr) -> Result<()> {
    if !name.starts_with(b"refs/") {
        return Err(metadata_error("ref name must start with refs/"));
    }
    gix::validate::reference::name(name).map_err(metadata_error)?;
    Ok(())
}

fn resolve<'a>(
    mut target: Target<'a>,
    loose: &BTreeMap<&'a BStr, Target<'a>>,
    packed: &BTreeMap<&BStr, GitOid>,
) -> Result<Option<GitOid>> {
    for _ in 0..MAX_SYMBOLIC_HOPS {
        match target {
            Target::Direct(oid) => return Ok(Some(oid)),
            Target::Symbolic(name) => match loose.get(name) {
                Some(&next) => target = next,
                None => return Ok(packed.get(name).copied()),
            },
        }
    }
    Err(metadata_error(
        "too many symbolic ref hops or a symbolic ref cycle",
    ))
}

fn metadata_error(error: impl std::fmt::Display) -> Error {
    Error::CorruptRecovery(format!("cannot determine Git object roots: {error}"))
}

#[derive(Default)]
struct Graph {
    pending: Vec<Root>,
    objects: BTreeMap<GitOid, Node>,
}

struct Node {
    kind: Option<ObjectKind>,
    content: Option<ContentId>,
}

impl Graph {
    fn require(&mut self, root @ Root(id, expected): Root) -> Result<()> {
        if id.as_bytes() == [0; 20] {
            return Err(Error::ObjectNotFound(id));
        }
        let full = self.objects.len() >= MAX_OBJECTS;
        match self.objects.entry(id) {
            Entry::Vacant(entry) if !full => {
                entry.insert(Node {
                    kind: expected,
                    content: None,
                });
                self.pending.push(root);
            }
            Entry::Vacant(_) => {
                return Err(Error::UnsupportedRepository(format!(
                    "Git object closure exceeds {MAX_OBJECTS} objects"
                )));
            }
            Entry::Occupied(mut entry) => {
                let node = entry.get_mut();
                if let Some(kind) = expected {
                    check_kind(id, node.kind, kind)?;
                    node.kind = Some(kind);
                }
            }
        }
        Ok(())
    }

    fn visit(&mut self, id: GitOid, kind: ObjectKind, bytes: &[u8]) -> Result<()> {
        let node = self.objects.get_mut(&id).expect("queued object has a node");
        check_kind(id, node.kind, kind)?;
        node.kind = Some(kind);
        self.children(kind, bytes)
    }

    fn children(&mut self, kind: ObjectKind, bytes: &[u8]) -> Result<()> {
        use gix::objs::{commit::ref_iter::Token, tree::EntryKind};
        let parse_error = |error: gix::objs::decode::Error| Error::CorruptObject(error.to_string());
        match kind {
            ObjectKind::Commit => {
                let mut found_tree = false;
                let mut found_committer = false;
                for token in gix::objs::CommitRefIter::from_bytes(bytes, HASH) {
                    let (id, kind) = match token.map_err(parse_error)? {
                        Token::Tree { id } => {
                            found_tree = true;
                            (id, ObjectKind::Tree)
                        }
                        Token::Parent { id } => (id, ObjectKind::Commit),
                        Token::Committer { .. } => {
                            found_committer = true;
                            continue;
                        }
                        _ => continue,
                    };
                    self.require(Root(raw_oid(id.as_slice())?, Some(kind)))?;
                }
                if !found_tree || !found_committer {
                    return Err(Error::CorruptObject(
                        "commit headers are incomplete".to_owned(),
                    ));
                }
            }
            ObjectKind::Tree => {
                for entry in gix::objs::TreeRefIter::from_bytes(bytes, HASH) {
                    let entry = entry.map_err(parse_error)?;
                    let kind = match entry.mode.kind() {
                        EntryKind::Tree => ObjectKind::Tree,
                        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => {
                            ObjectKind::Blob
                        }
                        EntryKind::Commit => {
                            return Err(Error::UnsupportedRepository(
                                "historical Git trees containing submodule links are not supported"
                                    .to_owned(),
                            ));
                        }
                    };
                    self.require(Root(raw_oid(entry.oid.as_bytes())?, Some(kind)))?;
                }
            }
            ObjectKind::Tag => {
                let tag = gix::objs::TagRef::from_bytes(bytes, HASH).map_err(parse_error)?;
                let target = GitOid::from_hex(tag.target.as_ref())?;
                self.require(Root(
                    target,
                    Some(crate::store::from_gix_kind(tag.target_kind)),
                ))?;
            }
            ObjectKind::Blob => {}
        }
        Ok(())
    }
}

fn raw_oid(bytes: &[u8]) -> Result<GitOid> {
    let oid = bytes
        .try_into()
        .map_err(|_| metadata_error("expected SHA-1 object ID"))?;
    Ok(GitOid::from_bytes(oid))
}

fn check_kind(id: GitOid, expected: Option<ObjectKind>, actual: ObjectKind) -> Result<()> {
    if let Some(expected) = expected
        && actual != expected
    {
        return Err(Error::CorruptObject(format!(
            "Git object {id} must be {expected}, not {actual}"
        )));
    }
    Ok(())
}

fn walk(
    roots: impl IntoIterator<Item = Root>,
    mut read: impl FnMut(GitOid, &mut Graph) -> Result<(ObjectKind, ContentId)>,
) -> Result<Vec<RetainedObject>> {
    let mut graph = Graph::default();
    for root in roots {
        graph.require(root)?;
    }
    while let Some(Root(oid, _)) = graph.pending.pop() {
        let (kind, content) = read(oid, &mut graph)?;
        graph.objects.insert(
            oid,
            Node {
                kind: Some(kind),
                content: Some(content),
            },
        );
    }
    Ok(graph
        .objects
        .into_iter()
        .map(|(oid, node)| RetainedObject {
            oid,
            content: node.content.expect("every queued object was read"),
        })
        .collect())
}
