//! Memory-bounded reads of objects stored in local Git pack files.
//!
//! gix caps each individual allocation derived from pack metadata, but the
//! aggregate size of all inflated delta instructions in a chain needs its own
//! cap. Before decoding, limit the chain length and instruction bytes. gix can
//! also use two object-sized scratch buffers while applying the deltas.

use std::ffi::OsStr;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use gix_pack::Bundle;
use gix_pack::cache::Never;
use gix_pack::data::entry::Header;

use crate::object::{GitOid, RawObject};
use crate::{Error, Result};

/// Longest delta chain accepted before an object is refused.
const MAX_DELTA_DEPTH: usize = 64;

/// Indexed packs in one Git object directory.
pub(super) struct PackedObjects {
    bundles: Vec<Bundle>,
    limit: usize,
}

impl PackedObjects {
    /// Open every indexed pack under `objects_dir`, or an empty reader when the
    /// pack directory is absent.
    ///
    /// Only real directories and regular files are accepted: a symlink in any
    /// final component is refused so a repository cannot read outside its own
    /// object database.
    ///
    /// # Errors
    ///
    /// Returns an error if a pack path is not local, or a pack or index cannot
    /// be opened.
    pub(super) fn open(objects_dir: &Path, limit: usize) -> Result<Self> {
        let packs_dir = objects_dir.join("pack");
        let metadata = match fs::symlink_metadata(&packs_dir) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self {
                    bundles: Vec::new(),
                    limit,
                });
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_dir() {
            return Err(Error::UnsupportedRepository(format!(
                "{} must be a real directory",
                packs_dir.display()
            )));
        }

        let mut indices = Vec::new();
        for entry in fs::read_dir(&packs_dir)? {
            let path = entry?.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                return Err(Error::UnsupportedRepository(format!(
                    "{} must not be a symlink",
                    path.display()
                )));
            }
            if !metadata.is_file() || path.extension() != Some(OsStr::new("idx")) {
                continue;
            }
            indices.push(path);
        }
        indices.sort();

        let mut bundles = Vec::with_capacity(indices.len());
        for index_path in indices {
            let pack_path = index_path.with_extension("pack");
            let pack_metadata = match fs::symlink_metadata(&pack_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !pack_metadata.file_type().is_file() {
                return Err(Error::UnsupportedRepository(format!(
                    "{} must be a regular pack file",
                    pack_path.display()
                )));
            }
            let Bundle { pack, index } =
                Bundle::at(&index_path, gix::hash::Kind::Sha1).map_err(gix_error)?;
            bundles.push(Bundle {
                pack: pack.with_alloc_limit_bytes(Some(limit)),
                index,
            });
        }
        Ok(Self { bundles, limit })
    }

    /// Read one object, or return `None` when no pack contains it.
    ///
    /// # Errors
    ///
    /// Returns an error if the object's delta chain is cyclic, unresolved,
    /// deeper than the depth limit, or needs more inflated delta instructions
    /// than the byte limit allows.
    pub(super) fn read(&self, id: GitOid) -> Result<Option<RawObject>> {
        let oid = gix::hash::ObjectId::from(id.as_bytes());
        for bundle in &self.bundles {
            let Some(index) = bundle.index.lookup(oid) else {
                continue;
            };
            self.check_chain(bundle, index)?;

            let mut out = Vec::new();
            let mut inflate = gix::features::zlib::Inflate::default();
            let mut cache = Never;
            let kind = {
                let (object, _) = bundle
                    .get_object_by_index(index, &mut out, &mut inflate, &mut cache)
                    .map_err(gix_error)?;
                super::from_gix_kind(object.kind)
            };
            return Ok(Some(RawObject { kind, data: out }));
        }
        Ok(None)
    }

    /// Verify a delta chain before gix allocates for it.
    ///
    /// gix bounds the declared base and result sizes it reads from delta
    /// headers, but sums the decompressed sizes of every delta entry into one
    /// instruction buffer. This walk applies the caller's limit to that sum and
    /// follows only bases inside the same pack, which is exactly what gix will
    /// resolve.
    fn check_chain(&self, bundle: &Bundle, index: u32) -> Result<()> {
        let budget = u64::try_from(self.limit).unwrap_or(u64::MAX);
        let mut offset = bundle.index.pack_offset_at_index(index);
        let mut visited = Vec::new();
        let mut deltas = 0;
        let mut instructions = 0_u64;

        loop {
            if visited.contains(&offset) {
                return Err(Error::CorruptObject(
                    "packed delta chain revisits an entry".to_owned(),
                ));
            }
            visited.push(offset);

            let entry = bundle.pack.entry(offset).map_err(gix_error)?;
            match entry.header {
                Header::Blob | Header::Tree | Header::Commit | Header::Tag => return Ok(()),
                Header::OfsDelta { base_distance } => {
                    instructions = add_instructions(instructions, entry.decompressed_size, budget)?;
                    offset = entry
                        .checked_base_pack_offset(base_distance)
                        .ok_or_else(|| {
                            Error::CorruptObject(
                                "packed ofs-delta base lies before the pack start".to_owned(),
                            )
                        })?;
                }
                Header::RefDelta { base_id } => {
                    instructions = add_instructions(instructions, entry.decompressed_size, budget)?;
                    let base_index = bundle.index.lookup(base_id.as_ref()).ok_or_else(|| {
                        Error::CorruptObject(format!(
                            "packed ref-delta base {base_id} is not in this pack"
                        ))
                    })?;
                    offset = bundle.index.pack_offset_at_index(base_index);
                }
            }

            deltas += 1;
            if deltas > MAX_DELTA_DEPTH {
                return Err(Error::CorruptObject(format!(
                    "packed delta chain is deeper than {MAX_DELTA_DEPTH} deltas"
                )));
            }
        }
    }
}

fn add_instructions(total: u64, size: u64, budget: u64) -> Result<u64> {
    let total = total.checked_add(size).ok_or_else(|| {
        Error::CorruptObject("packed delta instruction sizes overflow".to_owned())
    })?;
    if total > budget {
        return Err(Error::UnsupportedRepository(format!(
            "packed delta chain needs {total} bytes of instructions, above the {budget} byte retention limit"
        )));
    }
    Ok(total)
}

fn gix_error(error: impl std::fmt::Display) -> Error {
    Error::Gix(error.to_string())
}
