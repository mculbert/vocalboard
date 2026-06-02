//! Label / LabelKind — the unit stored as a Kind::Label blob (track 0).
//!
//! The in-memory types are the LATEST format. `mod v1` holds the frozen V1
//! wire schema (currently field-identical to the in-memory types) with
//! explicit conversions; future V2 introduces `mod v2` and evolves the
//! in-memory types, while `mod v1` stays untouched so old projects stay
//! readable indefinitely (lazy migration; see data-model.md § Schema version).

use serde::{Deserialize, Serialize};

use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};
use super::tilable::Tilable;

/// Format version emitted by every new [`encode_label`] call.
///
/// [`decode_label`] reads any version present in the dispatch table; only `1` is
/// known in M1.
pub const LATEST_LABEL_VERSION: u8 = 1;

/// A label track entry: the unit stored as a [`Kind::Label`] blob on track 0.
///
/// Position-independent: contains nothing about where it sits on the timeline,
/// so an unchanged label keeps the same hash regardless of edits elsewhere.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Label {
    /// Persistent label ID assigned at creation, stable across all later edits.
    pub id: u64,
    /// Displayed label text.
    pub text: String,
    /// Classification of this label.
    pub kind: LabelKind,
    /// Gap after this label (to the next element on track 0), in project-rate samples.
    pub post_label_silence: i64,
}

/// Classification of a [`Label`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelKind {
    /// A plain inline annotation.
    Plain,
    /// A section header dividing the recording into named segments.
    Section,
}

impl Tilable for Label {
    fn total_duration(&self) -> i64 {
        self.post_label_silence
    }
}

/// Encode `label` as the latest Label-kind wire format.
///
/// Always emits `(Kind::Label, LATEST_LABEL_VERSION)`. Returns the
/// content-addressing hash of the tagged bytes and the tagged-bytes blob
/// itself, ready for `store::put`.
pub fn encode_label(label: &Label) -> Result<(Hash, Vec<u8>), postcard::Error> {
    let v1 = v1::LabelV1::from(label);
    encode_tagged(Kind::Label, LATEST_LABEL_VERSION, &v1)
}

/// Decode a Kind::Label blob into the latest in-memory [`Label`].
///
/// Verifies the tag is `Kind::Label`, dispatches on the version nibble, and
/// upgrades through `From<LabelV{N}> for Label`. Unknown versions return
/// [`DecodeError::UnknownVersion`]; non-Label tags return [`DecodeError::KindMismatch`].
pub fn decode_label(bytes: &[u8]) -> Result<Label, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (_, version) = parse_tag(bytes[0])?;
    match version {
        1 => {
            let (_, v1_label): (u8, v1::LabelV1) = decode_tagged_as(Kind::Label, bytes)?;
            Ok(Label::from(v1_label))
        }
        _ => Err(DecodeError::UnknownVersion {
            kind: Kind::Label,
            version,
        }),
    }
}

/// V1 wire schema. Field-identical to the M1 in-memory types.
///
/// **Pre-1.0:** MAY be revised if implementation surfaces a missing or wrong
/// field; every revision requires regenerating the pinned hex/hash tests and
/// any committed G1 fixtures, and SHOULD bump `min_app_version`.
/// **Post-1.0:** frozen indefinitely — no field reorders, no enum-variant
/// reorders, no field insertions/deletions. Shape changes go through a new
/// `mod v2`, bumping `LATEST_LABEL_VERSION`, and writing `From<LabelV2> for Label`.
pub mod v1 {
    use serde::{Deserialize, Serialize};

    /// Frozen V1 wire representation of a [`super::Label`].
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub struct LabelV1 {
        /// Persistent label ID.
        pub id: u64,
        /// Displayed text.
        pub text: String,
        /// Label classification.
        pub kind: LabelKindV1,
        /// Post-label gap in project-rate samples.
        pub post_label_silence: i64,
    }

    /// Frozen V1 label-kind classification enum.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum LabelKindV1 {
        /// A plain inline annotation.
        Plain,
        /// A section header.
        Section,
    }
}

// --- Conversions between in-memory types and V1 wire types ---

impl From<v1::LabelKindV1> for LabelKind {
    fn from(v: v1::LabelKindV1) -> Self {
        match v {
            v1::LabelKindV1::Plain => LabelKind::Plain,
            v1::LabelKindV1::Section => LabelKind::Section,
        }
    }
}

impl From<LabelKind> for v1::LabelKindV1 {
    fn from(v: LabelKind) -> Self {
        match v {
            LabelKind::Plain => v1::LabelKindV1::Plain,
            LabelKind::Section => v1::LabelKindV1::Section,
        }
    }
}

impl From<v1::LabelV1> for Label {
    fn from(v: v1::LabelV1) -> Self {
        Label {
            id: v.id,
            text: v.text,
            kind: v.kind.into(),
            post_label_silence: v.post_label_silence,
        }
    }
}

impl From<&Label> for v1::LabelV1 {
    fn from(v: &Label) -> Self {
        v1::LabelV1 {
            id: v.id,
            text: v.text.clone(),
            kind: v.kind.into(),
            post_label_silence: v.post_label_silence,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::hash::{encode_tagged, tag_byte, DecodeError, Kind};

    fn sample_label() -> Label {
        Label {
            id: 3,
            text: "Chapter One".into(),
            kind: LabelKind::Section,
            post_label_silence: 22050,
        }
    }

    fn sample_v1_label() -> v1::LabelV1 {
        v1::LabelV1 {
            id: 1,
            text: "intro".into(),
            kind: v1::LabelKindV1::Plain,
            post_label_silence: 44100,
        }
    }

    #[test]
    fn store_load_round_trip() {
        let label = sample_label();
        let (_, bytes) = encode_label(&label).unwrap();
        let decoded = decode_label(&bytes).unwrap();
        assert_eq!(decoded, label);
    }

    #[test]
    fn empty_text_round_trip() {
        let label = Label {
            id: 5,
            text: String::new(),
            kind: LabelKind::Plain,
            post_label_silence: 0,
        };
        let (_, bytes) = encode_label(&label).unwrap();
        assert_eq!(decode_label(&bytes).unwrap(), label);
    }

    #[test]
    fn each_label_kind_round_trips() {
        let plain = Label {
            id: 1,
            text: "plain".into(),
            kind: LabelKind::Plain,
            post_label_silence: 100,
        };
        let section = Label {
            id: 2,
            text: "section".into(),
            kind: LabelKind::Section,
            post_label_silence: 200,
        };
        let (_, pb) = encode_label(&plain).unwrap();
        let (_, sb) = encode_label(&section).unwrap();
        assert_eq!(decode_label(&pb).unwrap().kind, LabelKind::Plain);
        assert_eq!(decode_label(&sb).unwrap().kind, LabelKind::Section);
    }

    #[test]
    fn extreme_value_samples_round_trip() {
        let label = Label {
            id: 99,
            text: "x".into(),
            kind: LabelKind::Plain,
            post_label_silence: i64::MAX,
        };
        let (_, bytes) = encode_label(&label).unwrap();
        assert_eq!(decode_label(&bytes).unwrap(), label);
    }

    #[test]
    fn hash_determinism() {
        let label = sample_label();
        let (h1, bytes1) = encode_label(&label).unwrap();
        let (h2, bytes2) = encode_label(&label).unwrap();
        assert_eq!(bytes1, bytes2);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_id() {
        let mut l1 = sample_label();
        let mut l2 = sample_label();
        l1.id = 10;
        l2.id = 11;
        let (h1, _) = encode_label(&l1).unwrap();
        let (h2, _) = encode_label(&l2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_text() {
        let mut l1 = sample_label();
        let mut l2 = sample_label();
        l1.text = "alpha".into();
        l2.text = "beta".into();
        let (h1, _) = encode_label(&l1).unwrap();
        let (h2, _) = encode_label(&l2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_kind() {
        let mut l1 = sample_label();
        let mut l2 = sample_label();
        l1.kind = LabelKind::Plain;
        l2.kind = LabelKind::Section;
        let (h1, _) = encode_label(&l1).unwrap();
        let (h2, _) = encode_label(&l2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn tag_byte_is_label_v1() {
        let label = sample_label();
        let (_, bytes) = encode_label(&label).unwrap();
        assert_eq!(bytes[0], 0x61, "first byte must be tag_byte(Label, 1)");
    }

    #[test]
    fn decode_label_kind_mismatch() {
        let (_, bytes) = encode_tagged(Kind::Turn, 1, &42u32).unwrap();
        let err = decode_label(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::KindMismatch {
                expected: Kind::Label,
                found: Kind::Turn,
            }
        ));
    }

    #[test]
    fn decode_label_unknown_version() {
        let tag = tag_byte(Kind::Label, 0xF);
        let bytes = [tag, 0x00, 0x00, 0x00, 0x00];
        let err = decode_label(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::UnknownVersion {
                kind: Kind::Label,
                version: 0xF,
            }
        ));
    }

    #[test]
    fn decode_label_empty_input() {
        let err = decode_label(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    #[test]
    fn decode_label_truncated_input() {
        let err = decode_label(&[0x61]).unwrap_err();
        assert!(matches!(err, DecodeError::Postcard(_)));
    }

    #[test]
    fn v1_conversions_total_round_trip() {
        let label = sample_label();
        let restored = Label::from(v1::LabelV1::from(&label));
        assert_eq!(restored, label);
    }

    #[test]
    fn v1_wire_format_pinned() {
        let v1_label = sample_v1_label();
        let (_, bytes) = encode_tagged(Kind::Label, 1, &v1_label).unwrap();
        // Captured via capture_pinned_values; regenerate if LabelV1 shape changes.
        let expected: &[u8] = &PINNED_WIRE_BYTES;
        assert_eq!(
            bytes.as_slice(),
            expected,
            "V1 wire format changed — regenerate pinned bytes"
        );
    }

    #[test]
    fn v1_hash_pinned() {
        let v1_label = sample_v1_label();
        let (hash, _) = encode_tagged(Kind::Label, 1, &v1_label).unwrap();
        assert_eq!(
            hash.0, PINNED_HASH,
            "V1 hash changed — regenerate pinned hash"
        );
    }

    #[test]
    fn tilable_total_duration_label() {
        let label = Label {
            id: 1,
            text: String::new(),
            kind: LabelKind::Plain,
            post_label_silence: 44100,
        };
        assert_eq!(label.total_duration(), 44100);
    }

    // Captured via capture_pinned_values after initial implementation.
    // Regenerate if LabelV1 / LabelKindV1 shape or postcard encoding changes.
    const PINNED_WIRE_BYTES: [u8; 12] = [
        0x61, 0x01, 0x05, 0x69, 0x6e, 0x74, 0x72, 0x6f, 0x00, 0x88, 0xb1, 0x05,
    ];
    const PINNED_HASH: [u8; 16] = [
        0x69, 0xdb, 0x8c, 0x6a, 0x0e, 0xe8, 0x9d, 0x8c, 0x5d, 0x0b, 0x2f, 0x74, 0x74, 0xb3, 0xcd,
        0xfb,
    ];

    // Helper to capture pinned values during initial implementation.
    // Run with: cargo test -p core label::tests::capture_pinned_values -- --ignored --nocapture
    #[test]
    #[ignore]
    fn capture_pinned_values() {
        let v1_label = sample_v1_label();
        let (hash, bytes) = encode_tagged(Kind::Label, 1, &v1_label).unwrap();
        println!("PINNED_WIRE_BYTES len={}", bytes.len());
        println!("bytes: {:?}", bytes);
        print!("const PINNED_WIRE_BYTES: [u8; {}] = [", bytes.len());
        for (i, b) in bytes.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("0x{b:02x}");
        }
        println!("];");
        print!("const PINNED_HASH: [u8; 16] = [");
        for (i, b) in hash.0.iter().enumerate() {
            if i > 0 {
                print!(", ");
            }
            print!("0x{b:02x}");
        }
        println!("];");
    }
}
