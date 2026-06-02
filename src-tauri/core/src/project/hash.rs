//! Content-addressing primitives: 128-bit BLAKE3 hash, kind+version tag byte,
//! and generic postcard encode/decode helpers.
//!
//! The tag byte packs object kind (high nibble) and format version (low nibble)
//! into a single byte: `tag = (kind << 4) | version`. Hashing always covers the
//! full tagged bytes so that blobs with the same content but different tags produce
//! different hashes.
//!
//! Per-kind typed loaders and writers (`decode_turn`, `encode_turn`, …) live with their
//! structs in later steps; they call [`encode_tagged`] / [`decode_tagged_as`] for the
//! common plumbing and maintain their own version dispatch tables.

use serde::{de::DeserializeOwned, Serialize};

/// Width of the content hash in bytes (128 bits / 16 bytes).
pub const HASH_BYTES: usize = 16;

/// A 128-bit BLAKE3 content hash.
///
/// Stored as `BLOB PRIMARY KEY` in the `store` table. Always computed over the
/// full tagged bytes (tag byte ++ postcard payload).
#[derive(Copy, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Hash(pub [u8; HASH_BYTES]);

impl std::fmt::Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Object kind stored in the content-addressed blob store.
///
/// Packed into the **high nibble** of the format tag byte: `tag = (kind << 4) | version`.
/// 16 kind slots (0x1–0xF usable; 0x0 reserved). If more than 15 kinds are needed,
/// the two-byte tag extension is the documented escape path.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A transcript turn (id, words, splices, timing).
    Turn = 0x1,
    /// Global project/track/speaker metadata blob.
    Metadata = 0x2,
    /// Full timeline snapshot (ordered turn-hash sequence per track).
    Snapshot = 0x3,
    /// Resampled room-tone PCM (f32 samples at the project sample rate).
    RoomTonePcm = 0x4,
    /// Normalised mean speaker embedding vector (f32).
    Embedding = 0x5,
    /// A label track entry (id, text, kind, post-label silence).
    Label = 0x6,
}

/// Errors returned by tag-parsing and blob-decode helpers.
#[derive(Debug)]
pub enum DecodeError {
    /// The input slice was empty; no tag byte is present.
    Empty,
    /// The high nibble of the tag byte is not a known [`Kind`].
    UnknownKind(u8),
    /// The version nibble names a format version this application does not handle.
    ///
    /// Per-kind loaders return this when their version dispatch table has no arm
    /// for the found version.
    UnknownVersion {
        /// Kind that was successfully identified.
        kind: Kind,
        /// Unrecognised version number.
        version: u8,
    },
    /// The tag's kind does not match the kind this typed loader expected.
    KindMismatch {
        /// Kind the caller required.
        expected: Kind,
        /// Kind found in the tag byte.
        found: Kind,
    },
    /// Postcard deserialization of the payload bytes failed.
    Postcard(postcard::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Empty => write!(f, "empty blob: no tag byte"),
            DecodeError::UnknownKind(n) => write!(f, "unknown kind nibble 0x{n:x}"),
            DecodeError::UnknownVersion { kind, version } => {
                write!(f, "unknown version {version} for kind {kind:?}")
            }
            DecodeError::KindMismatch { expected, found } => {
                write!(f, "kind mismatch: expected {expected:?}, found {found:?}")
            }
            DecodeError::Postcard(e) => write!(f, "postcard error: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DecodeError::Postcard(e) => Some(e),
            _ => None,
        }
    }
}

impl From<postcard::Error> for DecodeError {
    fn from(e: postcard::Error) -> Self {
        DecodeError::Postcard(e)
    }
}

/// Returns the tag byte for `kind` at `version`: `(kind as u8) << 4 | version`.
///
/// # Panics (debug builds only)
/// Asserts that `version` fits in a nibble (`<= 0x0F`).
pub fn tag_byte(kind: Kind, version: u8) -> u8 {
    debug_assert!(version <= 0x0F, "version must fit in a nibble (0–15)");
    ((kind as u8) << 4) | (version & 0x0F)
}

/// Parses a tag byte into its `(Kind, version)` components.
///
/// Returns [`DecodeError::UnknownKind`] if the high nibble is not a known [`Kind`].
pub fn parse_tag(b: u8) -> Result<(Kind, u8), DecodeError> {
    let kind_nibble = b >> 4;
    let version = b & 0x0F;
    let kind = match kind_nibble {
        0x1 => Kind::Turn,
        0x2 => Kind::Metadata,
        0x3 => Kind::Snapshot,
        0x4 => Kind::RoomTonePcm,
        0x5 => Kind::Embedding,
        0x6 => Kind::Label,
        other => return Err(DecodeError::UnknownKind(other)),
    };
    Ok((kind, version))
}

/// Computes the BLAKE3-128 hash of `bytes`.
///
/// `bytes` must be the **full tagged blob** (tag byte ++ postcard payload) so the
/// hash covers both the format tag and the serialized content.
pub fn hash_tagged(bytes: &[u8]) -> Hash {
    let digest = blake3::hash(bytes);
    let mut h = [0u8; HASH_BYTES];
    h.copy_from_slice(&digest.as_bytes()[..HASH_BYTES]);
    Hash(h)
}

/// Serializes `value` with postcard, prepends the tag byte, and returns `(hash, tagged_bytes)`.
///
/// Callers should pass their kind's current version constant so new writes always
/// produce the latest format.
///
/// # Errors
/// Returns a [`postcard::Error`] if serialization fails (extremely rare for the
/// well-formed project types encoded by this system).
pub fn encode_tagged<T: Serialize>(
    kind: Kind,
    version: u8,
    value: &T,
) -> Result<(Hash, Vec<u8>), postcard::Error> {
    let tag = tag_byte(kind, version);
    let payload = postcard::to_stdvec(value)?;
    let mut bytes = Vec::with_capacity(1 + payload.len());
    bytes.push(tag);
    bytes.extend_from_slice(&payload);
    let hash = hash_tagged(&bytes);
    Ok((hash, bytes))
}

/// Decodes a tagged blob, returning `(kind, version, value)`.
///
/// Does **not** validate the version; per-kind loaders are responsible for
/// dispatching on the version and returning [`DecodeError::UnknownVersion`] when
/// their dispatch table has no arm for it.
///
/// # Errors
/// - [`DecodeError::Empty`] — slice is empty.
/// - [`DecodeError::UnknownKind`] — kind nibble is not a known [`Kind`].
/// - [`DecodeError::Postcard`] — payload deserialization failed (e.g. truncated data).
pub fn decode_tagged<T: DeserializeOwned>(bytes: &[u8]) -> Result<(Kind, u8, T), DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (kind, version) = parse_tag(bytes[0])?;
    let value = postcard::from_bytes(&bytes[1..])?;
    Ok((kind, version, value))
}

/// Decodes a tagged blob, asserting the tag's kind matches `expected`.
///
/// On success returns `(version, value)`; callers are responsible for validating
/// the version against their per-kind dispatch table and returning
/// [`DecodeError::UnknownVersion`] for unrecognised versions.
///
/// # Errors
/// - [`DecodeError::Empty`] — slice is empty.
/// - [`DecodeError::UnknownKind`] — kind nibble is not a known [`Kind`].
/// - [`DecodeError::KindMismatch`] — tag kind differs from `expected`.
/// - [`DecodeError::Postcard`] — payload deserialization failed.
pub fn decode_tagged_as<T: DeserializeOwned>(
    expected: Kind,
    bytes: &[u8],
) -> Result<(u8, T), DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (found, version) = parse_tag(bytes[0])?;
    if found != expected {
        return Err(DecodeError::KindMismatch { expected, found });
    }
    let value = postcard::from_bytes(&bytes[1..])?;
    Ok((version, value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Thing {
        x: u32,
        s: String,
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Ordered {
        map: std::collections::BTreeMap<String, u64>,
    }

    const ALL_KINDS: &[Kind] = &[
        Kind::Turn,
        Kind::Metadata,
        Kind::Snapshot,
        Kind::RoomTonePcm,
        Kind::Embedding,
        Kind::Label,
    ];

    /// Tag round-trip: for every Kind and version 0..=15, parse_tag(tag_byte(k, v)) == (k, v).
    #[test]
    fn tag_round_trip() {
        for &kind in ALL_KINDS {
            for version in 0u8..=15 {
                let b = tag_byte(kind, version);
                let (k, v) = parse_tag(b).unwrap();
                assert_eq!(k, kind);
                assert_eq!(v, version);
            }
        }
    }

    /// Tag layout pinned: specific byte values are locked so a later edit cannot
    /// silently reshuffle on-disk codes.
    #[test]
    fn tag_layout_pinned() {
        assert_eq!(tag_byte(Kind::Turn, 1), 0x11);
        assert_eq!(tag_byte(Kind::Metadata, 1), 0x21);
        assert_eq!(tag_byte(Kind::Snapshot, 1), 0x31);
        assert_eq!(tag_byte(Kind::RoomTonePcm, 1), 0x41);
        assert_eq!(tag_byte(Kind::Embedding, 1), 0x51);
        assert_eq!(tag_byte(Kind::Label, 1), 0x61);
    }

    /// Hash determinism: the same struct encoded twice produces byte-identical output
    /// and the same Hash.
    #[test]
    fn hash_determinism() {
        let val = Thing {
            x: 42,
            s: "hello".into(),
        };
        let (h1, bytes1) = encode_tagged(Kind::Turn, 1, &val).unwrap();
        let (h2, bytes2) = encode_tagged(Kind::Turn, 1, &val).unwrap();
        assert_eq!(bytes1, bytes2);
        assert_eq!(h1, h2);
    }

    /// Hash covers tag: changing the tag byte while keeping postcard bytes constant
    /// changes the Hash.
    #[test]
    fn hash_covers_tag() {
        let val = Thing {
            x: 1,
            s: "same".into(),
        };
        let (h_turn, _) = encode_tagged(Kind::Turn, 1, &val).unwrap();
        let (h_meta, _) = encode_tagged(Kind::Metadata, 1, &val).unwrap();
        assert_ne!(h_turn, h_meta);

        let (h_v1, _) = encode_tagged(Kind::Turn, 1, &val).unwrap();
        let (h_v2, _) = encode_tagged(Kind::Turn, 2, &val).unwrap();
        assert_ne!(h_v1, h_v2);
    }

    /// Hash width: HASH_BYTES == 16 and the computed hash has that length.
    #[test]
    fn hash_width() {
        assert_eq!(HASH_BYTES, 16);
        let val = Thing {
            x: 0,
            s: String::new(),
        };
        let (h, _) = encode_tagged(Kind::Turn, 1, &val).unwrap();
        assert_eq!(h.0.len(), HASH_BYTES);
    }

    /// Encode/decode round-trip: decode_tagged(encode_tagged(k, v, &x).1) == (k, v, x).
    #[test]
    fn encode_decode_round_trip() {
        let val = Thing {
            x: 99,
            s: "world".into(),
        };
        let (_, bytes) = encode_tagged(Kind::Snapshot, 1, &val).unwrap();
        let (kind, version, decoded): (Kind, u8, Thing) = decode_tagged(&bytes).unwrap();
        assert_eq!(kind, Kind::Snapshot);
        assert_eq!(version, 1);
        assert_eq!(decoded, val);
    }

    /// Kind mismatch: decoding bytes tagged Snapshot as a Turn returns KindMismatch.
    #[test]
    fn kind_mismatch() {
        let val = Thing {
            x: 1,
            s: "x".into(),
        };
        let (_, bytes) = encode_tagged(Kind::Snapshot, 1, &val).unwrap();
        let err = decode_tagged_as::<Thing>(Kind::Turn, &bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::KindMismatch {
                expected: Kind::Turn,
                found: Kind::Snapshot
            }
        ));
    }

    /// Unknown version: documents the UnknownVersion error shape used by per-kind
    /// loaders (turn.rs step 4+) when their dispatch table has no arm for a version.
    /// parse_tag passes version 0xF through; the per-kind match arm produces the error.
    #[test]
    fn unknown_version_variant() {
        // parse_tag surfaces the version nibble cleanly — rejection is per-kind.
        let b = tag_byte(Kind::Turn, 0xF);
        let (kind, version) = parse_tag(b).unwrap();
        assert_eq!(kind, Kind::Turn);
        assert_eq!(version, 0xF);
        // Construct the error a per-kind loader would return for this version.
        let err = DecodeError::UnknownVersion { kind, version };
        assert!(matches!(
            err,
            DecodeError::UnknownVersion {
                kind: Kind::Turn,
                version: 0xF
            }
        ));
    }

    /// Hash::Debug formats as lowercase hex with no separators.
    #[test]
    fn hash_debug_format() {
        let h = Hash([
            0x00, 0x01, 0x0f, 0x10, 0xff, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc,
            0xde, 0xf0,
        ]);
        assert_eq!(format!("{h:?}"), "00010f10ffabcdef123456789abcdef0");
    }

    /// DecodeError::Display produces human-readable messages for every variant.
    #[test]
    fn decode_error_display_messages() {
        let empty = DecodeError::Empty.to_string();
        assert!(empty.contains("empty"), "Empty: {empty}");

        let unknown_kind = DecodeError::UnknownKind(0xA).to_string();
        assert!(unknown_kind.contains('a'), "UnknownKind: {unknown_kind}");

        let unknown_ver = DecodeError::UnknownVersion {
            kind: Kind::Turn,
            version: 5,
        }
        .to_string();
        assert!(
            unknown_ver.contains('5') && unknown_ver.contains("Turn"),
            "UnknownVersion: {unknown_ver}"
        );

        let mismatch = DecodeError::KindMismatch {
            expected: Kind::Turn,
            found: Kind::Metadata,
        }
        .to_string();
        assert!(
            mismatch.contains("Turn") && mismatch.contains("Metadata"),
            "KindMismatch: {mismatch}"
        );

        // Postcard variant: a truncated payload produces a real postcard::Error.
        let err = decode_tagged::<Thing>(&[tag_byte(Kind::Turn, 1)]).unwrap_err();
        let postcard_msg = err.to_string();
        assert!(
            postcard_msg.contains("postcard"),
            "Postcard display: {postcard_msg}"
        );
    }

    /// DecodeError::source() returns None for all non-Postcard variants and
    /// Some(...) for the Postcard variant.
    #[test]
    fn decode_error_source_impl() {
        use std::error::Error;

        assert!(DecodeError::Empty.source().is_none());
        assert!(DecodeError::UnknownKind(0).source().is_none());
        assert!(DecodeError::UnknownVersion {
            kind: Kind::Turn,
            version: 0
        }
        .source()
        .is_none());
        assert!(DecodeError::KindMismatch {
            expected: Kind::Turn,
            found: Kind::Metadata
        }
        .source()
        .is_none());

        let postcard_err = decode_tagged::<Thing>(&[tag_byte(Kind::Turn, 1)]).unwrap_err();
        assert!(
            postcard_err.source().is_some(),
            "Postcard variant must chain the inner error"
        );
    }

    /// Empty payload: decode_tagged(&[]) returns Empty.
    #[test]
    fn empty_payload() {
        let err = decode_tagged::<Thing>(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    /// Truncated payload: just the tag byte (no postcard data) returns Postcard error.
    #[test]
    fn truncated_payload() {
        let tag = tag_byte(Kind::Turn, 1);
        let err = decode_tagged::<Thing>(&[tag]).unwrap_err();
        assert!(matches!(err, DecodeError::Postcard(_)));
    }

    /// Postcard determinism guard: a struct containing a BTreeMap encodes identically
    /// across calls (confirming callers using ordered collections get deterministic bytes).
    #[test]
    fn btreemap_determinism() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("z".to_string(), 3u64);
        map.insert("a".to_string(), 1u64);
        map.insert("m".to_string(), 2u64);
        let val = Ordered { map };
        let (h1, bytes1) = encode_tagged(Kind::Metadata, 1, &val).unwrap();
        let (h2, bytes2) = encode_tagged(Kind::Metadata, 1, &val).unwrap();
        assert_eq!(bytes1, bytes2);
        assert_eq!(h1, h2);
    }
}
