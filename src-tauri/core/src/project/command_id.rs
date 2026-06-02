//! Bit-mask category codes naming the command category that produced a journal row.

/// Command-category code stamped on every journal row's `command_id` column.
///
/// Each value is a **bit-mask flag**: OR-ing the codes across a journal range
/// gives the set of command categories touched in that range (the future
/// history-view feature). The integer value of each variant is an **on-disk
/// code** — written into `journal.command_id` and read back across sessions — so
/// codes are **permanent and append-only**: never renumber an existing variant,
/// never reuse a retired bit.
///
/// Bit 0 (`0x1`) is the **Undo flag**: an undo of a category-`X` command is
/// stamped `X | 0x1` (e.g. [`CommandId::UndoCut`] `== Cut as i64 | 0x1`). A redo
/// re-stamps the plain category code. `0x0` ([`CommandId::Unknown`]) is a row
/// tied to no edit category, e.g. a standalone snapshot.
#[repr(i64)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CommandId {
    /// No edit category — e.g. an automated or on-demand snapshot. (M1 stamps
    /// this on the `new_project` initial snapshot and every `save_snapshot_now`.)
    Unknown = 0x0,
    /// Generic undo (the undo flag with no specific category).
    Undo = 0x1,
    /// Cut: cut words / section / crop, cut disfluencies / sounds / breaths,
    /// declick-by-cut. On a `type = -1` metadata row, indicates remove-track.
    Cut = 0x2,
    /// Undo of [`CommandId::Cut`].
    UndoCut = 0x3,
    /// Mute (replace with room tone): mute words / disfluencies / sounds /
    /// breaths, declick-by-mute.
    Mute = 0x4,
    /// Undo of [`CommandId::Mute`].
    UndoMute = 0x5,
    /// Edit transcript text (and transcript formatting, if implemented).
    EditText = 0x8,
    /// Undo of [`CommandId::EditText`].
    UndoEditText = 0x9,
    /// Edit a label: add / delete / modify / move / change kind; also rename-track.
    EditLabel = 0x10,
    /// Undo of [`CommandId::EditLabel`].
    UndoEditLabel = 0x11,
    /// Edit a speaker: rename, reassign a turn, merge / delete speakers, re-detect.
    EditSpeaker = 0x20,
    /// Undo of [`CommandId::EditSpeaker`].
    UndoEditSpeaker = 0x21,
    /// Insert: add a speech / non-speech track, paste section, reorder turn,
    /// add room tone, concatenate projects.
    Insert = 0x40,
    /// Undo of [`CommandId::Insert`].
    UndoInsert = 0x41,
    /// Record audio (incl. punch-and-roll).
    RecordAudio = 0x80,
    /// Undo of [`CommandId::RecordAudio`].
    UndoRecordAudio = 0x81,
    /// Adjust spacing: word / turn spacing (manual or automated), align tracks,
    /// split / merge turns.
    AdjustSpacing = 0x100,
    /// Undo of [`CommandId::AdjustSpacing`].
    UndoAdjustSpacing = 0x101,
    /// Adjust levels: track / section / turn levels, ramps, smart ducking,
    /// enhance wet/dry mix, channel fade, crosstalk attenuation, peak normalize.
    AdjustLevels = 0x200,
    /// Undo of [`CommandId::AdjustLevels`].
    UndoAdjustLevels = 0x201,
    /// Adjust EQ (manual or automatic).
    AdjustEq = 0x400,
    /// Undo of [`CommandId::AdjustEq`].
    UndoAdjustEq = 0x401,
    /// Correct speech: in-painting, pace adjustment via resynthesis.
    CorrectSpeech = 0x800,
    /// Undo of [`CommandId::CorrectSpeech`].
    UndoCorrectSpeech = 0x801,
    /// Audio effects: reverb / echo and similar.
    AudioEffects = 0x1000,
    /// Undo of [`CommandId::AudioEffects`].
    UndoAudioEffects = 0x1001,
    /// Separate overlapping speech (over-talk) within a track.
    SeparateOvertalk = 0x2000,
    /// Undo of [`CommandId::SeparateOvertalk`].
    UndoSeparateOvertalk = 0x2001,
}

/// The Undo-flag bit ORed into a category code to mark an undo row.
pub const UNDO_FLAG: i64 = 0x1;

impl CommandId {
    /// The stable on-disk code for this command category.
    pub fn code(self) -> i64 {
        self as i64
    }

    /// The undo-stamp for a forward command category: the category with
    /// [`UNDO_FLAG`] set (e.g. `Cut.undo_of() == UndoCut`; `Unknown.undo_of() ==
    /// Undo`). Defined for every single category; falls back to `Undo` for a
    /// non-category code (which commands never pass).
    pub fn undo_of(self) -> CommandId {
        CommandId::from_code(self.code() | UNDO_FLAG).unwrap_or(CommandId::Undo)
    }

    /// Map an on-disk code back to a [`CommandId`], or `None` if it is not a
    /// single defined category — an unknown bit (a newer app version), or a
    /// combined mask such as `Cut | Mute` produced by OR-ing several rows.
    pub fn from_code(code: i64) -> Option<Self> {
        use CommandId::*;
        Some(match code {
            0x0 => Unknown,
            0x1 => Undo,
            0x2 => Cut,
            0x3 => UndoCut,
            0x4 => Mute,
            0x5 => UndoMute,
            0x8 => EditText,
            0x9 => UndoEditText,
            0x10 => EditLabel,
            0x11 => UndoEditLabel,
            0x20 => EditSpeaker,
            0x21 => UndoEditSpeaker,
            0x40 => Insert,
            0x41 => UndoInsert,
            0x80 => RecordAudio,
            0x81 => UndoRecordAudio,
            0x100 => AdjustSpacing,
            0x101 => UndoAdjustSpacing,
            0x200 => AdjustLevels,
            0x201 => UndoAdjustLevels,
            0x400 => AdjustEq,
            0x401 => UndoAdjustEq,
            0x800 => CorrectSpeech,
            0x801 => UndoCorrectSpeech,
            0x1000 => AudioEffects,
            0x1001 => UndoAudioEffects,
            0x2000 => SeparateOvertalk,
            0x2001 => UndoSeparateOvertalk,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // C1
    #[test]
    #[allow(clippy::cognitive_complexity)]
    fn codes_are_pinned() {
        assert_eq!(CommandId::Unknown.code(), 0x0);
        assert_eq!(CommandId::Undo.code(), 0x1);
        assert_eq!(CommandId::Cut.code(), 0x2);
        assert_eq!(CommandId::UndoCut.code(), 0x3);
        assert_eq!(CommandId::Mute.code(), 0x4);
        assert_eq!(CommandId::UndoMute.code(), 0x5);
        assert_eq!(CommandId::EditText.code(), 0x8);
        assert_eq!(CommandId::UndoEditText.code(), 0x9);
        assert_eq!(CommandId::EditLabel.code(), 0x10);
        assert_eq!(CommandId::UndoEditLabel.code(), 0x11);
        assert_eq!(CommandId::EditSpeaker.code(), 0x20);
        assert_eq!(CommandId::UndoEditSpeaker.code(), 0x21);
        assert_eq!(CommandId::Insert.code(), 0x40);
        assert_eq!(CommandId::UndoInsert.code(), 0x41);
        assert_eq!(CommandId::RecordAudio.code(), 0x80);
        assert_eq!(CommandId::UndoRecordAudio.code(), 0x81);
        assert_eq!(CommandId::AdjustSpacing.code(), 0x100);
        assert_eq!(CommandId::UndoAdjustSpacing.code(), 0x101);
        assert_eq!(CommandId::AdjustLevels.code(), 0x200);
        assert_eq!(CommandId::UndoAdjustLevels.code(), 0x201);
        assert_eq!(CommandId::AdjustEq.code(), 0x400);
        assert_eq!(CommandId::UndoAdjustEq.code(), 0x401);
        assert_eq!(CommandId::CorrectSpeech.code(), 0x800);
        assert_eq!(CommandId::UndoCorrectSpeech.code(), 0x801);
        assert_eq!(CommandId::AudioEffects.code(), 0x1000);
        assert_eq!(CommandId::UndoAudioEffects.code(), 0x1001);
        assert_eq!(CommandId::SeparateOvertalk.code(), 0x2000);
        assert_eq!(CommandId::UndoSeparateOvertalk.code(), 0x2001);
        assert_eq!(UNDO_FLAG, 0x1);
    }

    // C2
    #[test]
    fn code_round_trips() {
        let all = [
            CommandId::Unknown,
            CommandId::Undo,
            CommandId::Cut,
            CommandId::UndoCut,
            CommandId::Mute,
            CommandId::UndoMute,
            CommandId::EditText,
            CommandId::UndoEditText,
            CommandId::EditLabel,
            CommandId::UndoEditLabel,
            CommandId::EditSpeaker,
            CommandId::UndoEditSpeaker,
            CommandId::Insert,
            CommandId::UndoInsert,
            CommandId::RecordAudio,
            CommandId::UndoRecordAudio,
            CommandId::AdjustSpacing,
            CommandId::UndoAdjustSpacing,
            CommandId::AdjustLevels,
            CommandId::UndoAdjustLevels,
            CommandId::AdjustEq,
            CommandId::UndoAdjustEq,
            CommandId::CorrectSpeech,
            CommandId::UndoCorrectSpeech,
            CommandId::AudioEffects,
            CommandId::UndoAudioEffects,
            CommandId::SeparateOvertalk,
            CommandId::UndoSeparateOvertalk,
        ];
        for c in all {
            assert_eq!(
                CommandId::from_code(c.code()),
                Some(c),
                "from_code(code({c:?})) should return Some({c:?})"
            );
        }
    }

    // C3
    #[test]
    fn undo_flag_relationship() {
        let pairs = [
            (CommandId::Cut, CommandId::UndoCut),
            (CommandId::Mute, CommandId::UndoMute),
            (CommandId::EditText, CommandId::UndoEditText),
            (CommandId::EditLabel, CommandId::UndoEditLabel),
            (CommandId::EditSpeaker, CommandId::UndoEditSpeaker),
            (CommandId::Insert, CommandId::UndoInsert),
            (CommandId::RecordAudio, CommandId::UndoRecordAudio),
            (CommandId::AdjustSpacing, CommandId::UndoAdjustSpacing),
            (CommandId::AdjustLevels, CommandId::UndoAdjustLevels),
            (CommandId::AdjustEq, CommandId::UndoAdjustEq),
            (CommandId::CorrectSpeech, CommandId::UndoCorrectSpeech),
            (CommandId::AudioEffects, CommandId::UndoAudioEffects),
            (CommandId::SeparateOvertalk, CommandId::UndoSeparateOvertalk),
        ];
        for (base, undo) in pairs {
            assert_eq!(
                undo.code(),
                base.code() | UNDO_FLAG,
                "Undo{base:?}.code() should equal {base:?}.code() | UNDO_FLAG"
            );
        }
    }

    // C5
    #[test]
    fn undo_of_maps_forward_to_undo_variant() {
        assert_eq!(CommandId::Cut.undo_of(), CommandId::UndoCut);
        assert_eq!(CommandId::Mute.undo_of(), CommandId::UndoMute);
        assert_eq!(CommandId::Unknown.undo_of(), CommandId::Undo);
    }

    // C6
    #[test]
    fn undo_of_is_idempotent_on_undo_variants() {
        assert_eq!(CommandId::UndoCut.undo_of(), CommandId::UndoCut);
        assert_eq!(CommandId::UndoMute.undo_of(), CommandId::UndoMute);
        assert_eq!(CommandId::Undo.undo_of(), CommandId::Undo);
    }

    // C4
    #[test]
    fn from_code_unknown_or_combined_is_none() {
        assert!(CommandId::from_code(0x4000).is_none(), "unallocated bit");
        assert!(
            CommandId::from_code(0x6).is_none(),
            "combined mask Cut|Mute=0x6 is not a single variant"
        );
        assert!(CommandId::from_code(-1).is_none(), "forward-compat: -1");
        assert_eq!(
            CommandId::from_code(0x0),
            Some(CommandId::Unknown),
            "0x0 is the defined Unknown variant"
        );
    }
}
