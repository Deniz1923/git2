//! Byte-safe representations of Git object data.

use std::fmt;
use std::str::FromStr;

use bstr::BString;
use sha1::{Digest, Sha1};

use crate::{Error, Result};

const SHA1_LEN: usize = 20;
const SHA1_HEX_LEN: usize = SHA1_LEN * 2;

/// A validated SHA-1 Git object identifier.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitOid([u8; SHA1_LEN]);

impl GitOid {
    /// Construct an identifier from its raw digest.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; SHA1_LEN]) -> Self {
        Self(bytes)
    }

    /// Return the raw SHA-1 digest.
    #[must_use]
    pub const fn as_bytes(self) -> [u8; SHA1_LEN] {
        self.0
    }

    /// Parse a full 40-character hexadecimal object identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidOid`] if `hex` is not exactly 40 hexadecimal bytes.
    pub fn from_hex(hex: &[u8]) -> Result<Self> {
        if hex.len() != SHA1_HEX_LEN {
            return Err(Error::InvalidOid(format!(
                "expected {SHA1_HEX_LEN} hexadecimal characters, got {}",
                hex.len()
            )));
        }

        let mut bytes = [0_u8; SHA1_LEN];
        for (index, pair) in hex.as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = (decode_nibble(pair[0])? << 4) | decode_nibble(pair[1])?;
        }
        Ok(Self(bytes))
    }

    /// Format the identifier as lowercase hexadecimal.
    #[must_use]
    pub fn to_hex(self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(SHA1_HEX_LEN);
        for byte in self.0 {
            output.push(char::from(HEX[usize::from(byte >> 4)]));
            output.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        output
    }
}

impl fmt::Debug for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("GitOid")
            .field(&self.to_hex())
            .finish()
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for GitOid {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::from_hex(value.as_bytes())
    }
}

fn decode_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(Error::InvalidOid(format!(
            "invalid hexadecimal byte 0x{byte:02x}"
        ))),
    }
}

/// The four object kinds supported by Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectKind {
    /// Return the canonical object-header spelling.
    #[must_use]
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Blob => b"blob",
            Self::Tree => b"tree",
            Self::Commit => b"commit",
            Self::Tag => b"tag",
        }
    }

    /// Parse the object-header spelling.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] for an unknown object kind.
    pub fn from_bytes(value: &[u8]) -> Result<Self> {
        match value {
            b"blob" => Ok(Self::Blob),
            b"tree" => Ok(Self::Tree),
            b"commit" => Ok(Self::Commit),
            b"tag" => Ok(Self::Tag),
            _ => Err(Error::CorruptObject(format!(
                "unknown object kind {:?}",
                BString::from(value)
            ))),
        }
    }
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(std::str::from_utf8(self.as_bytes()).expect("object kinds are ASCII"))
    }
}

/// A decoded Git object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObject {
    pub kind: ObjectKind,
    pub data: Vec<u8>,
}

impl RawObject {
    /// Compute the identifier Git assigns to this object.
    #[must_use]
    pub fn id(&self) -> GitOid {
        hash_object(self.kind, &self.data)
    }

    /// Produce the bytes Git hashes and compresses for a loose object.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        canonical_object_bytes(self.kind, &self.data)
    }
}

/// Produce the canonical `"<kind> <length>\0<data>"` byte sequence.
#[must_use]
pub fn canonical_object_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let length = data.len().to_string();
    let mut output = Vec::with_capacity(kind.as_bytes().len() + length.len() + data.len() + 2);
    output.extend_from_slice(kind.as_bytes());
    output.push(b' ');
    output.extend_from_slice(length.as_bytes());
    output.push(0);
    output.extend_from_slice(data);
    output
}

/// Hash object data exactly as Git does for SHA-1 repositories.
#[must_use]
pub fn hash_object(kind: ObjectKind, data: &[u8]) -> GitOid {
    let digest = Sha1::digest(canonical_object_bytes(kind, data));
    let mut bytes = [0_u8; SHA1_LEN];
    bytes.copy_from_slice(&digest);
    GitOid(bytes)
}

/// Parse uncompressed canonical loose-object bytes.
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if the header, kind, length, or payload is malformed.
pub fn parse_canonical_object(bytes: &[u8]) -> Result<RawObject> {
    let nul = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| Error::CorruptObject("object header has no NUL terminator".to_owned()))?;
    let header = &bytes[..nul];
    let space = header
        .iter()
        .position(|byte| *byte == b' ')
        .ok_or_else(|| Error::CorruptObject("object header has no length".to_owned()))?;
    let kind = ObjectKind::from_bytes(&header[..space])?;
    let declared = std::str::from_utf8(&header[space + 1..])
        .map_err(|_| Error::CorruptObject("object length is not ASCII".to_owned()))?
        .parse::<usize>()
        .map_err(|_| Error::CorruptObject("object length is invalid".to_owned()))?;
    let data = &bytes[nul + 1..];
    if declared != data.len() {
        return Err(Error::CorruptObject(format!(
            "object declares {declared} bytes but contains {}",
            data.len()
        )));
    }
    Ok(RawObject {
        kind,
        data: data.to_vec(),
    })
}

/// A byte-safe Git identity and timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: BString,
    pub email: BString,
    pub timestamp: i64,
    pub timezone_offset_minutes: i16,
}

impl Signature {
    /// Parse the value of an `author` or `committer` header.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] if identity, timestamp, or timezone fields are malformed.
    pub fn parse(value: &[u8]) -> Result<Self> {
        let email_end = value
            .iter()
            .rposition(|byte| *byte == b'>')
            .ok_or_else(|| Error::CorruptObject("signature has no closing '>'".to_owned()))?;
        let email_start = value[..email_end]
            .iter()
            .rposition(|byte| *byte == b'<')
            .ok_or_else(|| Error::CorruptObject("signature has no opening '<'".to_owned()))?;
        let name = trim_ascii_end(&value[..email_start]);
        let tail = trim_ascii_start(&value[email_end + 1..]);
        let mut fields = tail.split(u8::is_ascii_whitespace);
        let timestamp = fields
            .next()
            .ok_or_else(|| Error::CorruptObject("signature has no timestamp".to_owned()))?;
        let timezone = fields
            .find(|field| !field.is_empty())
            .ok_or_else(|| Error::CorruptObject("signature has no timezone".to_owned()))?;

        let timestamp = std::str::from_utf8(timestamp)
            .map_err(|_| Error::CorruptObject("signature timestamp is not ASCII".to_owned()))?
            .parse()
            .map_err(|_| Error::CorruptObject("signature timestamp is invalid".to_owned()))?;

        Ok(Self {
            name: BString::from(name),
            email: BString::from(&value[email_start + 1..email_end]),
            timestamp,
            timezone_offset_minutes: parse_timezone(timezone)?,
        })
    }

    /// Serialize a canonical author/committer header value.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut output = Vec::new();
        output.extend_from_slice(self.name.as_ref());
        output.extend_from_slice(b" <");
        output.extend_from_slice(self.email.as_ref());
        output.extend_from_slice(b"> ");
        output.extend_from_slice(self.timestamp.to_string().as_bytes());
        output.push(b' ');
        output.extend_from_slice(format_timezone(self.timezone_offset_minutes).as_bytes());
        output
    }
}

fn parse_timezone(value: &[u8]) -> Result<i16> {
    if value.len() != 5
        || !matches!(value[0], b'+' | b'-')
        || !value[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(Error::CorruptObject(
            "signature timezone is invalid".to_owned(),
        ));
    }
    let hours = i16::from(value[1] - b'0') * 10 + i16::from(value[2] - b'0');
    let minutes = i16::from(value[3] - b'0') * 10 + i16::from(value[4] - b'0');
    if hours > 23 || minutes > 59 {
        return Err(Error::CorruptObject(
            "signature timezone is out of range".to_owned(),
        ));
    }
    let offset = hours * 60 + minutes;
    Ok(if value[0] == b'-' { -offset } else { offset })
}

fn format_timezone(offset: i16) -> String {
    let sign = if offset < 0 { '-' } else { '+' };
    let absolute = offset.unsigned_abs();
    format!("{sign}{:02}{:02}", absolute / 60, absolute % 60)
}

fn trim_ascii_start(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    bytes
}

fn trim_ascii_end(mut bytes: &[u8]) -> &[u8] {
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// A commit object with unknown headers retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub tree: GitOid,
    pub parents: Vec<GitOid>,
    pub author: Signature,
    pub committer: Signature,
    pub extra_headers: Vec<(BString, BString)>,
    pub message: BString,
}

impl Commit {
    /// Parse canonical commit object data (without the loose-object header).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] if required headers are missing or any header is invalid.
    pub fn parse(data: &[u8]) -> Result<Self> {
        let separator = data
            .windows(2)
            .position(|window| window == b"\n\n")
            .ok_or_else(|| Error::CorruptObject("commit has no message separator".to_owned()))?;
        let headers = parse_headers(&data[..separator])?;
        let message = BString::from(&data[separator + 2..]);

        let mut tree = None;
        let mut parents = Vec::new();
        let mut author = None;
        let mut committer = None;
        let mut extra_headers = Vec::new();

        for (name, value) in headers {
            let name_bytes: &[u8] = name.as_ref();
            match name_bytes {
                b"tree" => tree = Some(GitOid::from_hex(value.as_ref())?),
                b"parent" => parents.push(GitOid::from_hex(value.as_ref())?),
                b"author" => author = Some(Signature::parse(value.as_ref())?),
                b"committer" => committer = Some(Signature::parse(value.as_ref())?),
                _ => extra_headers.push((name, value)),
            }
        }

        Ok(Self {
            tree: tree.ok_or_else(|| Error::CorruptObject("commit has no tree".to_owned()))?,
            parents,
            author: author
                .ok_or_else(|| Error::CorruptObject("commit has no author".to_owned()))?,
            committer: committer
                .ok_or_else(|| Error::CorruptObject("commit has no committer".to_owned()))?,
            extra_headers,
            message,
        })
    }

    /// Serialize canonical commit object data.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut output = Vec::new();
        write_header(&mut output, b"tree", self.tree.to_hex().as_bytes());
        for parent in &self.parents {
            write_header(&mut output, b"parent", parent.to_hex().as_bytes());
        }
        write_header(&mut output, b"author", &self.author.serialize());
        write_header(&mut output, b"committer", &self.committer.serialize());
        for (name, value) in &self.extra_headers {
            write_header(&mut output, name.as_ref(), value.as_ref());
        }
        output.push(b'\n');
        output.extend_from_slice(self.message.as_ref());
        output
    }
}

fn parse_headers(data: &[u8]) -> Result<Vec<(BString, BString)>> {
    let mut headers: Vec<(BString, BString)> = Vec::new();
    for line in data.split(|byte| *byte == b'\n') {
        if let Some(continuation) = line.strip_prefix(b" ") {
            let (_, value) = headers
                .last_mut()
                .ok_or_else(|| Error::CorruptObject("orphaned header continuation".to_owned()))?;
            value.push(b'\n');
            value.extend_from_slice(continuation);
            continue;
        }
        let space = line
            .iter()
            .position(|byte| *byte == b' ')
            .ok_or_else(|| Error::CorruptObject("commit header has no value".to_owned()))?;
        headers.push((
            BString::from(&line[..space]),
            BString::from(&line[space + 1..]),
        ));
    }
    Ok(headers)
}

fn write_header(output: &mut Vec<u8>, name: &[u8], value: &[u8]) {
    output.extend_from_slice(name);
    output.push(b' ');
    let mut lines = value.split(|byte| *byte == b'\n');
    if let Some(first) = lines.next() {
        output.extend_from_slice(first);
    }
    for continuation in lines {
        output.extend_from_slice(b"\n ");
        output.extend_from_slice(continuation);
    }
    output.push(b'\n');
}

/// A mode stored in a tree entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeMode {
    File,
    Executable,
    Symlink,
    Directory,
    Gitlink,
}

impl TreeMode {
    #[must_use]
    pub const fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::File => b"100644",
            Self::Executable => b"100755",
            Self::Symlink => b"120000",
            Self::Directory => b"40000",
            Self::Gitlink => b"160000",
        }
    }

    fn from_bytes(value: &[u8]) -> Result<Self> {
        match value {
            b"100644" => Ok(Self::File),
            b"100755" => Ok(Self::Executable),
            b"120000" => Ok(Self::Symlink),
            b"40000" | b"040000" => Ok(Self::Directory),
            b"160000" => Ok(Self::Gitlink),
            _ => Err(Error::CorruptObject(format!(
                "unsupported tree mode {:?}",
                BString::from(value)
            ))),
        }
    }
}

/// One byte-safe tree entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub mode: TreeMode,
    pub name: BString,
    pub oid: GitOid,
}

/// A Git directory snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Parse binary tree object data.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CorruptObject`] if an entry has an invalid mode, name, or object ID.
    pub fn parse(mut data: &[u8]) -> Result<Self> {
        let mut entries = Vec::new();
        while !data.is_empty() {
            let space = data
                .iter()
                .position(|byte| *byte == b' ')
                .ok_or_else(|| Error::CorruptObject("tree entry has no mode".to_owned()))?;
            let mode = TreeMode::from_bytes(&data[..space])?;
            data = &data[space + 1..];
            let nul = data
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| Error::CorruptObject("tree entry has no NUL".to_owned()))?;
            let name = BString::from(&data[..nul]);
            data = &data[nul + 1..];
            if data.len() < SHA1_LEN {
                return Err(Error::CorruptObject(
                    "tree entry has a short object id".to_owned(),
                ));
            }
            let mut oid = [0_u8; SHA1_LEN];
            oid.copy_from_slice(&data[..SHA1_LEN]);
            data = &data[SHA1_LEN..];
            entries.push(TreeEntry {
                mode,
                name,
                oid: GitOid::from_bytes(oid),
            });
        }
        Ok(Self { entries })
    }

    /// Serialize binary tree object data, preserving entry order.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut output = Vec::new();
        for entry in &self.entries {
            output.extend_from_slice(entry.mode.as_bytes());
            output.push(b' ');
            output.extend_from_slice(entry.name.as_ref());
            output.push(0);
            output.extend_from_slice(&entry.oid.as_bytes());
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_the_empty_blob_like_git() {
        assert_eq!(
            hash_object(ObjectKind::Blob, b"").to_hex(),
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391"
        );
    }

    #[test]
    fn canonical_object_round_trips() {
        let object = RawObject {
            kind: ObjectKind::Blob,
            data: b"a\0b\n".to_vec(),
        };
        assert_eq!(
            parse_canonical_object(&object.canonical_bytes()).unwrap(),
            object
        );
    }

    #[test]
    fn commit_preserves_byte_messages_and_multiline_headers() {
        let source = concat!(
            "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n",
            "author A <a@example.com> 1 +0230\n",
            "committer C <c@example.com> 2 -0430\n",
            "gpgsig first\n",
            " second\n",
            "\n"
        )
        .as_bytes()
        .iter()
        .copied()
        .chain([0xff, b'\n'])
        .collect::<Vec<_>>();
        let commit = Commit::parse(&source).unwrap();
        let message: &[u8] = commit.message.as_ref();
        assert_eq!(message, &[0xff, b'\n']);
        assert_eq!(commit.author.timezone_offset_minutes, 150);
        assert_eq!(commit.committer.timezone_offset_minutes, -270);
        assert_eq!(commit.serialize(), source);
    }

    #[test]
    fn tree_round_trips_non_utf8_and_gitlinks() {
        let tree = Tree {
            entries: vec![
                TreeEntry {
                    mode: TreeMode::File,
                    name: BString::from(vec![b'a', 0xff]),
                    oid: GitOid::from_bytes([1; SHA1_LEN]),
                },
                TreeEntry {
                    mode: TreeMode::Gitlink,
                    name: BString::from("module"),
                    oid: GitOid::from_bytes([2; SHA1_LEN]),
                },
            ],
        };
        assert_eq!(Tree::parse(&tree.serialize()).unwrap(), tree);
    }
}
