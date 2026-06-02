//! Turn / Word / Splice — the unit stored as a Kind::Turn blob.
//!
//! The in-memory types are the LATEST format. `mod v1` holds the frozen V1
//! wire schema (currently field-identical to the in-memory types) with
//! explicit conversions; future V2 introduces `mod v2` and evolves the
//! in-memory types, while `mod v1` stays untouched so old projects stay
//! readable indefinitely (lazy migration; see data-model.md § Schema version).

use serde::{Deserialize, Serialize};

use super::hash::{decode_tagged_as, encode_tagged, parse_tag, DecodeError, Hash, Kind};
use super::tilable::Tilable;

/// Format version emitted by every new [`encode_turn`] call.
///
/// [`decode_turn`] reads any version present in the dispatch table; only `1` is
/// known in M1.
pub const LATEST_TURN_VERSION: u8 = 1;

/// A transcription turn: the unit stored as a [`Kind::Turn`] blob.
///
/// Position-independent: contains nothing about where it sits on the project
/// timeline, so an unchanged turn keeps the same hash regardless of edits elsewhere.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// Persistent turn ID assigned at creation, stable across all later edits.
    ///
    /// Part of the hashed payload so two turns with identical data but different
    /// timeline positions get different hashes (keeps the hash-keyed adjacency list
    /// and delta `location` references unambiguous).
    pub id: u64,
    /// Speaker ID; `None` for the "[None]" non-speech pseudo-speaker.
    pub speaker_id: Option<u64>,
    /// Speech duration, in integer samples at the project rate.
    pub turn_duration: i64,
    /// Gap after this turn (to the next turn or label), in project-rate samples.
    pub post_turn_silence: i64,
    /// Aligned words in this turn.
    pub words: Vec<Word>,
    /// Edit Decision List splices tiling `turn_duration + post_turn_silence`.
    pub splices: Vec<Splice>,
}

/// An aligned word within a [`Turn`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Word {
    /// Classification of this word token.
    pub word_type: WordType,
    /// Displayed text.
    pub text: String,
    /// Approximate start position in the source audio file, in seconds.
    pub start_sec: f64,
    /// Approximate end position in the source audio file, in seconds.
    pub end_sec: f64,
    /// Whether this word has been cut (removed from playback).
    pub is_cut: bool,
    /// Whether this word has been muted (silenced in playback).
    pub is_muted: bool,
    /// Word onset within this turn, in project-rate samples.
    pub turn_offset_sample: i64,
    /// Precise word length in project-rate samples (0 until zero-crossing is computed).
    pub length_samples: i64,
}

/// A single splice in a turn's Edit Decision List.
///
/// Splices tile the turn gaplessly: a splice's position within the turn is the
/// running sum of `length_samples` of all preceding splices.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Splice {
    /// Duration of this splice, in project-rate samples.
    pub length_samples: i64,
    /// Crossfade-in length, in project-rate samples.
    pub fade_in_samples: i64,
    /// Crossfade-out length, in project-rate samples.
    pub fade_out_samples: i64,
    /// Whether this splice draws from the source file, room tone, or synthesised silence.
    pub kind: SpliceKind,
}

/// Classification of a word token in a [`Turn`].
///
/// Labels and section headers are not `Word` kinds — they are [`Kind::Label`] blobs
/// on track 0 and live in `label.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WordType {
    /// Ordinary transcribed speech.
    Normal,
    /// Filler or hesitation ("um", "uh", …).
    Disfluency,
    /// Non-speech sound event (e.g. a YAMnet-labelled sound).
    Sound,
}

/// Source of audio for a [`Splice`] within a [`Turn`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpliceKind {
    /// Audio drawn from the original source file.
    Source {
        /// Sample offset of the first sample to read in the source file.
        source_start_sample: i64,
        /// Resampled samples to discard before playback begins.
        source_decode_offset: i64,
    },
    /// Audio drawn from the track's room-tone recording.
    RoomTone,
    /// Synthesised digital silence.
    Silence,
}

impl Tilable for Turn {
    fn total_duration(&self) -> i64 {
        self.turn_duration + self.post_turn_silence
    }
}

/// Encode `turn` as the latest Turn-kind wire format.
///
/// Always emits `(Kind::Turn, LATEST_TURN_VERSION)`. Returns the
/// content-addressing hash of the tagged bytes and the tagged-bytes blob
/// itself, ready for `store::put`.
pub fn encode_turn(turn: &Turn) -> Result<(Hash, Vec<u8>), postcard::Error> {
    let v1 = v1::TurnV1::from(turn);
    encode_tagged(Kind::Turn, LATEST_TURN_VERSION, &v1)
}

/// Decode a Kind::Turn blob into the latest in-memory [`Turn`].
///
/// Verifies the tag is `Kind::Turn`, dispatches on the version nibble, and
/// upgrades through `From<TurnV{N}> for Turn`. Unknown versions return
/// [`DecodeError::UnknownVersion`]; non-Turn tags return [`DecodeError::KindMismatch`].
pub fn decode_turn(bytes: &[u8]) -> Result<Turn, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Empty);
    }
    let (_, version) = parse_tag(bytes[0])?;
    match version {
        1 => {
            let (_, v1_turn): (u8, v1::TurnV1) = decode_tagged_as(Kind::Turn, bytes)?;
            Ok(Turn::from(v1_turn))
        }
        _ => Err(DecodeError::UnknownVersion {
            kind: Kind::Turn,
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
/// `mod v2`, bumping `LATEST_TURN_VERSION`, and writing `From<TurnV2> for Turn`.
pub mod v1 {
    use serde::{Deserialize, Serialize};

    /// Frozen V1 wire representation of a [`super::Turn`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct TurnV1 {
        /// Persistent turn ID.
        pub id: u64,
        /// Speaker ID; `None` for the non-speech pseudo-speaker.
        pub speaker_id: Option<u64>,
        /// Speech duration in project-rate samples.
        pub turn_duration: i64,
        /// Post-turn gap in project-rate samples.
        pub post_turn_silence: i64,
        /// Aligned words.
        pub words: Vec<WordV1>,
        /// EDL splices.
        pub splices: Vec<SpliceV1>,
    }

    /// Frozen V1 wire representation of a [`super::Word`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct WordV1 {
        /// Word classification.
        pub word_type: WordTypeV1,
        /// Displayed text.
        pub text: String,
        /// Approximate source-file start in seconds.
        pub start_sec: f64,
        /// Approximate source-file end in seconds.
        pub end_sec: f64,
        /// Whether this word is cut.
        pub is_cut: bool,
        /// Whether this word is muted.
        pub is_muted: bool,
        /// Word onset within the turn, in project-rate samples.
        pub turn_offset_sample: i64,
        /// Precise word length in project-rate samples.
        pub length_samples: i64,
    }

    /// Frozen V1 wire representation of a [`super::Splice`].
    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct SpliceV1 {
        /// Duration in project-rate samples.
        pub length_samples: i64,
        /// Crossfade-in in project-rate samples.
        pub fade_in_samples: i64,
        /// Crossfade-out in project-rate samples.
        pub fade_out_samples: i64,
        /// Splice source kind.
        pub kind: SpliceKindV1,
    }

    /// Frozen V1 word-type classification enum.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum WordTypeV1 {
        /// Ordinary transcribed speech.
        Normal,
        /// Filler or hesitation.
        Disfluency,
        /// Non-speech sound event.
        Sound,
    }

    /// Frozen V1 splice-kind enum.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
    pub enum SpliceKindV1 {
        /// Audio from the original source file.
        Source {
            /// Sample offset in the source file.
            source_start_sample: i64,
            /// Resampled samples to discard before playback.
            source_decode_offset: i64,
        },
        /// Audio from the room-tone recording.
        RoomTone,
        /// Synthesised digital silence.
        Silence,
    }
}

// --- Conversions between in-memory types and V1 wire types ---

impl From<v1::WordTypeV1> for WordType {
    fn from(v: v1::WordTypeV1) -> Self {
        match v {
            v1::WordTypeV1::Normal => WordType::Normal,
            v1::WordTypeV1::Disfluency => WordType::Disfluency,
            v1::WordTypeV1::Sound => WordType::Sound,
        }
    }
}

impl From<WordType> for v1::WordTypeV1 {
    fn from(v: WordType) -> Self {
        match v {
            WordType::Normal => v1::WordTypeV1::Normal,
            WordType::Disfluency => v1::WordTypeV1::Disfluency,
            WordType::Sound => v1::WordTypeV1::Sound,
        }
    }
}

impl From<v1::SpliceKindV1> for SpliceKind {
    fn from(v: v1::SpliceKindV1) -> Self {
        match v {
            v1::SpliceKindV1::Source {
                source_start_sample,
                source_decode_offset,
            } => SpliceKind::Source {
                source_start_sample,
                source_decode_offset,
            },
            v1::SpliceKindV1::RoomTone => SpliceKind::RoomTone,
            v1::SpliceKindV1::Silence => SpliceKind::Silence,
        }
    }
}

impl From<SpliceKind> for v1::SpliceKindV1 {
    fn from(v: SpliceKind) -> Self {
        match v {
            SpliceKind::Source {
                source_start_sample,
                source_decode_offset,
            } => v1::SpliceKindV1::Source {
                source_start_sample,
                source_decode_offset,
            },
            SpliceKind::RoomTone => v1::SpliceKindV1::RoomTone,
            SpliceKind::Silence => v1::SpliceKindV1::Silence,
        }
    }
}

impl From<v1::WordV1> for Word {
    fn from(v: v1::WordV1) -> Self {
        Word {
            word_type: v.word_type.into(),
            text: v.text,
            start_sec: v.start_sec,
            end_sec: v.end_sec,
            is_cut: v.is_cut,
            is_muted: v.is_muted,
            turn_offset_sample: v.turn_offset_sample,
            length_samples: v.length_samples,
        }
    }
}

impl From<&Word> for v1::WordV1 {
    fn from(v: &Word) -> Self {
        v1::WordV1 {
            word_type: v.word_type.into(),
            text: v.text.clone(),
            start_sec: v.start_sec,
            end_sec: v.end_sec,
            is_cut: v.is_cut,
            is_muted: v.is_muted,
            turn_offset_sample: v.turn_offset_sample,
            length_samples: v.length_samples,
        }
    }
}

impl From<v1::SpliceV1> for Splice {
    fn from(v: v1::SpliceV1) -> Self {
        Splice {
            length_samples: v.length_samples,
            fade_in_samples: v.fade_in_samples,
            fade_out_samples: v.fade_out_samples,
            kind: v.kind.into(),
        }
    }
}

impl From<&Splice> for v1::SpliceV1 {
    fn from(v: &Splice) -> Self {
        v1::SpliceV1 {
            length_samples: v.length_samples,
            fade_in_samples: v.fade_in_samples,
            fade_out_samples: v.fade_out_samples,
            kind: v.kind.into(),
        }
    }
}

impl From<v1::TurnV1> for Turn {
    fn from(v: v1::TurnV1) -> Self {
        Turn {
            id: v.id,
            speaker_id: v.speaker_id,
            turn_duration: v.turn_duration,
            post_turn_silence: v.post_turn_silence,
            words: v.words.into_iter().map(Word::from).collect(),
            splices: v.splices.into_iter().map(Splice::from).collect(),
        }
    }
}

impl From<&Turn> for v1::TurnV1 {
    fn from(v: &Turn) -> Self {
        v1::TurnV1 {
            id: v.id,
            speaker_id: v.speaker_id,
            turn_duration: v.turn_duration,
            post_turn_silence: v.post_turn_silence,
            words: v.words.iter().map(v1::WordV1::from).collect(),
            splices: v.splices.iter().map(v1::SpliceV1::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::hash::{encode_tagged, tag_byte, DecodeError, Kind};

    // Non-trivial turn: three words (Normal, Disfluency cut, Normal muted), two splices.
    fn sample_turn() -> Turn {
        Turn {
            id: 7,
            speaker_id: Some(3),
            turn_duration: 44100,
            post_turn_silence: 8820,
            words: vec![
                Word {
                    word_type: WordType::Normal,
                    text: "Hello".into(),
                    start_sec: 0.1,
                    end_sec: 0.4,
                    is_cut: false,
                    is_muted: false,
                    turn_offset_sample: 4410,
                    length_samples: 13230,
                },
                Word {
                    word_type: WordType::Disfluency,
                    text: "um".into(),
                    start_sec: 0.4,
                    end_sec: 0.6,
                    is_cut: true,
                    is_muted: false,
                    turn_offset_sample: 17640,
                    length_samples: 8820,
                },
                Word {
                    word_type: WordType::Normal,
                    text: "world".into(),
                    start_sec: 0.7,
                    end_sec: 1.0,
                    is_cut: false,
                    is_muted: true,
                    turn_offset_sample: 30870,
                    length_samples: 13230,
                },
            ],
            splices: vec![
                Splice {
                    length_samples: 44100,
                    fade_in_samples: 100,
                    fade_out_samples: 100,
                    kind: SpliceKind::Source {
                        source_start_sample: 88200,
                        source_decode_offset: 0,
                    },
                },
                Splice {
                    length_samples: 8820,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                },
            ],
        }
    }

    // Hand-constructed TurnV1 shared by the pinned wire-format and hash tests.
    fn sample_v1_turn() -> v1::TurnV1 {
        v1::TurnV1 {
            id: 1,
            speaker_id: Some(42),
            turn_duration: 44100,
            post_turn_silence: 8820,
            words: vec![v1::WordV1 {
                word_type: v1::WordTypeV1::Normal,
                text: "hello".into(),
                start_sec: 0.1,
                end_sec: 0.5,
                is_cut: false,
                is_muted: false,
                turn_offset_sample: 4410,
                length_samples: 17640,
            }],
            splices: vec![
                v1::SpliceV1 {
                    length_samples: 44100,
                    fade_in_samples: 100,
                    fade_out_samples: 100,
                    kind: v1::SpliceKindV1::Source {
                        source_start_sample: 1000,
                        source_decode_offset: 0,
                    },
                },
                v1::SpliceV1 {
                    length_samples: 8820,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: v1::SpliceKindV1::Silence,
                },
            ],
        }
    }

    #[test]
    fn store_load_round_trip() {
        let turn = sample_turn();
        let (_, bytes) = encode_turn(&turn).unwrap();
        let decoded = decode_turn(&bytes).unwrap();
        assert_eq!(decoded, turn);
    }

    #[test]
    fn empty_collections_round_trip() {
        let turn = Turn {
            id: 1,
            speaker_id: None,
            turn_duration: 0,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![],
        };
        let (_, bytes) = encode_turn(&turn).unwrap();
        assert_eq!(decode_turn(&bytes).unwrap(), turn);
    }

    #[test]
    fn sound_event_round_trip() {
        let turn = Turn {
            id: 20,
            speaker_id: None,
            turn_duration: 22050,
            post_turn_silence: 4410,
            words: vec![Word {
                word_type: WordType::Sound,
                text: "[Sound]".into(),
                start_sec: 1.0,
                end_sec: 1.5,
                is_cut: false,
                is_muted: false,
                turn_offset_sample: 0,
                length_samples: 22050,
            }],
            splices: vec![Splice {
                length_samples: 26460,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 44100,
                    source_decode_offset: 0,
                },
            }],
        };
        let (_, bytes) = encode_turn(&turn).unwrap();
        assert_eq!(decode_turn(&bytes).unwrap(), turn);
    }

    #[test]
    fn each_word_type_round_trips() {
        let make_word = |wt| Word {
            word_type: wt,
            text: format!("{wt:?}"),
            start_sec: 0.0,
            end_sec: 0.1,
            is_cut: false,
            is_muted: false,
            turn_offset_sample: 0,
            length_samples: 4410,
        };
        let turn = Turn {
            id: 5,
            speaker_id: Some(1),
            turn_duration: 22050,
            post_turn_silence: 0,
            words: vec![
                make_word(WordType::Normal),
                make_word(WordType::Disfluency),
                make_word(WordType::Sound),
            ],
            splices: vec![],
        };
        let (_, bytes) = encode_turn(&turn).unwrap();
        let decoded = decode_turn(&bytes).unwrap();
        let types: Vec<WordType> = decoded.words.iter().map(|w| w.word_type).collect();
        assert_eq!(
            types,
            [WordType::Normal, WordType::Disfluency, WordType::Sound,]
        );
    }

    #[test]
    fn each_splice_kind_round_trips() {
        let turn = Turn {
            id: 6,
            speaker_id: Some(2),
            turn_duration: 44100,
            post_turn_silence: 0,
            words: vec![],
            splices: vec![
                Splice {
                    length_samples: 22050,
                    fade_in_samples: 10,
                    fade_out_samples: 10,
                    kind: SpliceKind::Source {
                        source_start_sample: 100,
                        source_decode_offset: 50,
                    },
                },
                Splice {
                    length_samples: 11025,
                    fade_in_samples: 5,
                    fade_out_samples: 5,
                    kind: SpliceKind::RoomTone,
                },
                Splice {
                    length_samples: 11025,
                    fade_in_samples: 0,
                    fade_out_samples: 0,
                    kind: SpliceKind::Silence,
                },
            ],
        };
        let (_, bytes) = encode_turn(&turn).unwrap();
        let decoded = decode_turn(&bytes).unwrap();
        assert_eq!(
            decoded.splices[0].kind,
            SpliceKind::Source {
                source_start_sample: 100,
                source_decode_offset: 50,
            }
        );
        assert_eq!(decoded.splices[1].kind, SpliceKind::RoomTone);
        assert_eq!(decoded.splices[2].kind, SpliceKind::Silence);
    }

    #[test]
    fn extreme_value_samples_round_trip() {
        let turn = Turn {
            id: 99,
            speaker_id: Some(1),
            turn_duration: i64::MAX,
            post_turn_silence: 1,
            words: vec![Word {
                word_type: WordType::Normal,
                text: "x".into(),
                start_sec: 0.0,
                end_sec: 1.0,
                is_cut: false,
                is_muted: false,
                turn_offset_sample: 1,
                length_samples: i64::MAX,
            }],
            splices: vec![Splice {
                length_samples: i64::MAX,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: i64::MAX,
                    source_decode_offset: 1,
                },
            }],
        };
        let (_, bytes) = encode_turn(&turn).unwrap();
        assert_eq!(decode_turn(&bytes).unwrap(), turn);
    }

    #[test]
    fn hash_determinism() {
        let turn = sample_turn();
        let (h1, bytes1) = encode_turn(&turn).unwrap();
        let (h2, bytes2) = encode_turn(&turn).unwrap();
        assert_eq!(bytes1, bytes2);
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_id() {
        let mut t1 = sample_turn();
        let mut t2 = sample_turn();
        t1.id = 100;
        t2.id = 101;
        let (h1, _) = encode_turn(&t1).unwrap();
        let (h2, _) = encode_turn(&t2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_speaker() {
        let mut t1 = sample_turn();
        let mut t2 = sample_turn();
        t1.speaker_id = Some(1);
        t2.speaker_id = Some(2);
        let (h1, _) = encode_turn(&t1).unwrap();
        let (h2, _) = encode_turn(&t2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_word_text() {
        let mut t1 = sample_turn();
        let mut t2 = sample_turn();
        t1.words[0].text = "alpha".into();
        t2.words[0].text = "beta".into();
        let (h1, _) = encode_turn(&t1).unwrap();
        let (h2, _) = encode_turn(&t2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_sensitive_to_splice_source_offset() {
        let mut t1 = sample_turn();
        let mut t2 = sample_turn();
        t1.splices[0].kind = SpliceKind::Source {
            source_start_sample: 1000,
            source_decode_offset: 0,
        };
        t2.splices[0].kind = SpliceKind::Source {
            source_start_sample: 2000,
            source_decode_offset: 0,
        };
        let (h1, _) = encode_turn(&t1).unwrap();
        let (h2, _) = encode_turn(&t2).unwrap();
        assert_ne!(h1, h2);
    }

    #[test]
    fn tag_byte_is_turn_v1() {
        let turn = sample_turn();
        let (_, bytes) = encode_turn(&turn).unwrap();
        assert_eq!(bytes[0], 0x11, "first byte must be tag_byte(Turn, 1)");
    }

    #[test]
    fn decode_turn_kind_mismatch() {
        let (_, bytes) = encode_tagged(Kind::Snapshot, 1, &42u32).unwrap();
        let err = decode_turn(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::KindMismatch {
                expected: Kind::Turn,
                found: Kind::Snapshot,
            }
        ));
    }

    #[test]
    fn decode_turn_kind_mismatch_label() {
        let (_, bytes) = encode_tagged(Kind::Label, 1, &42u32).unwrap();
        let err = decode_turn(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::KindMismatch {
                expected: Kind::Turn,
                found: Kind::Label,
            }
        ));
    }

    #[test]
    fn decode_turn_unknown_version() {
        let tag = tag_byte(Kind::Turn, 0xF);
        let bytes = [tag, 0x00, 0x00, 0x00, 0x00];
        let err = decode_turn(&bytes).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::UnknownVersion {
                kind: Kind::Turn,
                version: 0xF,
            }
        ));
    }

    #[test]
    fn decode_turn_empty_input() {
        let err = decode_turn(&[]).unwrap_err();
        assert!(matches!(err, DecodeError::Empty));
    }

    #[test]
    fn decode_turn_truncated_input() {
        let err = decode_turn(&[0x11]).unwrap_err();
        assert!(matches!(err, DecodeError::Postcard(_)));
    }

    #[test]
    fn v1_conversions_total_round_trip() {
        let turn = sample_turn();
        let restored = Turn::from(v1::TurnV1::from(&turn));
        assert_eq!(restored, turn);
    }

    #[test]
    fn v1_wire_format_pinned() {
        let v1_turn = sample_v1_turn();
        let (_, bytes) = encode_tagged(Kind::Turn, 1, &v1_turn).unwrap();
        // Captured via capture_pinned_values; regenerate if TurnV1 shape changes.
        let expected: &[u8] = &PINNED_WIRE_BYTES;
        assert_eq!(
            bytes.as_slice(),
            expected,
            "V1 wire format changed — regenerate pinned bytes"
        );
    }

    #[test]
    fn hash_pinned_for_v1_sample() {
        let v1_turn = sample_v1_turn();
        let (hash, _) = encode_tagged(Kind::Turn, 1, &v1_turn).unwrap();
        assert_eq!(
            hash.0, PINNED_HASH,
            "V1 hash changed — regenerate pinned hash"
        );
    }

    #[test]
    fn tilable_total_duration_turn() {
        let turn = Turn {
            id: 1,
            speaker_id: None,
            turn_duration: 44100,
            post_turn_silence: 8820,
            words: vec![],
            splices: vec![],
        };
        assert_eq!(turn.total_duration(), 44100 + 8820);
    }

    // Captured via capture_pinned_values after revising TurnV1 shape.
    // Regenerate if TurnV1 / WordV1 / SpliceV1 shape or postcard encoding changes.
    const PINNED_WIRE_BYTES: [u8; 59] = [
        0x11, 0x01, 0x01, 0x2a, 0x88, 0xb1, 0x05, 0xe8, 0x89, 0x01, 0x01, 0x00, 0x05, 0x68, 0x65,
        0x6c, 0x6c, 0x6f, 0x9a, 0x99, 0x99, 0x99, 0x99, 0x99, 0xb9, 0x3f, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0xe0, 0x3f, 0x00, 0x00, 0xf4, 0x44, 0xd0, 0x93, 0x02, 0x02, 0x88, 0xb1, 0x05,
        0xc8, 0x01, 0xc8, 0x01, 0x00, 0xd0, 0x0f, 0x00, 0xe8, 0x89, 0x01, 0x00, 0x00, 0x02,
    ];
    const PINNED_HASH: [u8; 16] = [
        0x4b, 0x01, 0x18, 0x1f, 0x71, 0x68, 0xfd, 0xdf, 0xf6, 0x88, 0x6b, 0xd4, 0x0d, 0xeb, 0x25,
        0xa2,
    ];

    // Helper to capture pinned values after a shape revision.
    // Run with: cargo test -p core turn::tests::capture_pinned_values -- --ignored --nocapture
    #[test]
    #[ignore]
    fn capture_pinned_values() {
        let v1_turn = sample_v1_turn();
        let (hash, bytes) = encode_tagged(Kind::Turn, 1, &v1_turn).unwrap();
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
