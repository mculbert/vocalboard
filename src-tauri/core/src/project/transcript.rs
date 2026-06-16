//! Transcript export: render the project's turn trees as WebVTT or Markdown.
//!
//! This reads the timeline tree and touches no audio code, so it lives in `project/` rather than
//! `audio/`. Turns from all tracks stream through [`MergedTurns`](super::tree::MergedTurns) in
//! global timeline order; each formatter builds its output in a single pass (no intermediate
//! `Vec`-materialise-and-sort).

use std::collections::BTreeMap;
use std::path::Path;

use super::tree::{ImplicitTimelineTree, MergedTurns};
use super::turn::Turn;

/// Transcript export format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptFormat {
    /// WebVTT: one cue per turn with speaker voice tag and `HH:MM:SS.mmm` timestamps.
    Vtt,
    /// Markdown: one speaker-labelled paragraph per turn.
    Markdown,
}

/// Map an output-file extension to a [`TranscriptFormat`].
///
/// `.vtt` → VTT; `.md` / `.markdown` → Markdown (matched case-insensitively). Returns `None` for
/// unrecognised extensions (the handler maps `None → export_unsupported_format`, so this module
/// stays free of `AudioError`). Extension wins over any caller-supplied format hint
/// (design/audio-pipeline.md § Format selection).
pub fn transcript_format_for(path: &Path) -> Option<TranscriptFormat> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("vtt") => Some(TranscriptFormat::Vtt),
        Some("md") | Some("markdown") => Some(TranscriptFormat::Markdown),
        _ => None,
    }
}

/// Convert a project-timeline sample position to `HH:MM:SS.mmm`.
///
/// Uses integer (floor) arithmetic: `0` → `"00:00:00.000"`.
fn samples_to_timestamp(samples: i64, sample_rate: u32) -> String {
    let ms_total = samples * 1000 / sample_rate as i64;
    let ms = ms_total % 1000;
    let s_total = ms_total / 1000;
    let s = s_total % 60;
    let m_total = s_total / 60;
    let m = m_total % 60;
    let h = m_total / 60;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

/// Join the visible words in `turn`, honouring `include_cut_words`.
fn turn_words_text(turn: &Turn, include_cut_words: bool) -> String {
    turn.words
        .iter()
        .filter(|w| include_cut_words || !w.is_cut)
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Resolve a speaker ID to its display name, falling back to `"[None]"`.
fn speaker_name(speaker_id: Option<u64>, speakers: &BTreeMap<u64, String>) -> &str {
    speaker_id
        .and_then(|id| speakers.get(&id).map(|s| s.as_str()))
        .unwrap_or("[None]")
}

/// Render `turns` as WebVTT in a single pass.
fn fmt_vtt<'a>(
    turns: impl Iterator<Item = (i64, i64, &'a Turn)>,
    speakers: &BTreeMap<u64, String>,
    sample_rate: u32,
    include_cut_words: bool,
) -> String {
    let mut out = String::from("WEBVTT\n");
    for (start, end, turn) in turns {
        out.push('\n');
        out.push_str(&samples_to_timestamp(start, sample_rate));
        out.push_str(" --> ");
        out.push_str(&samples_to_timestamp(end, sample_rate));
        out.push('\n');
        out.push_str(&format!(
            "<v {}>{}\n",
            speaker_name(turn.speaker_id, speakers),
            turn_words_text(turn, include_cut_words),
        ));
    }
    out
}

/// Render `turns` as Markdown (one speaker-labelled paragraph each) in a single pass.
fn fmt_markdown<'a>(
    turns: impl Iterator<Item = (i64, i64, &'a Turn)>,
    speakers: &BTreeMap<u64, String>,
    include_cut_words: bool,
) -> String {
    let mut out = String::new();
    for (_, _, turn) in turns {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!(
            "**{}:** {}",
            speaker_name(turn.speaker_id, speakers),
            turn_words_text(turn, include_cut_words),
        ));
    }
    out
}

/// Format the transcript from `trees` as VTT or Markdown.
///
/// Each tree entry is `(project_start_sample, &tree)`; turns are positioned at
/// `project_start_sample + tree-local start_sample`, so tracks beginning at different project
/// offsets still merge in true global timeline order via [`MergedTurns`]. A single tree
/// degenerates to that tree's in-order iterator. `speakers` maps `speaker_id → display name`;
/// `None` speaker renders as `"[None]"`. `include_cut_words` controls whether `is_cut` words
/// appear. `sample_rate` is the project rate used to convert sample positions to `HH:MM:SS.mmm`
/// timestamps (VTT only).
pub fn format_transcript(
    trees: &[(i64, &ImplicitTimelineTree<Turn>)],
    speakers: &BTreeMap<u64, String>,
    sample_rate: u32,
    format: TranscriptFormat,
    include_cut_words: bool,
) -> String {
    let turns = MergedTurns::new(trees);
    match format {
        TranscriptFormat::Vtt => fmt_vtt(turns, speakers, sample_rate, include_cut_words),
        TranscriptFormat::Markdown => fmt_markdown(turns, speakers, include_cut_words),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::project::turn::{encode_turn, Splice, SpliceKind, Turn, Word, WordType};

    /// Build a single-turn tree for transcript tests.
    fn make_tx_tree(
        turn_id: u64,
        speaker_id: Option<u64>,
        turn_duration: i64,
        post_turn_silence: i64,
        words: &[(&str, bool)], // (text, is_cut)
    ) -> ImplicitTimelineTree<Turn> {
        let total = turn_duration + post_turn_silence;
        let turn = Turn {
            id: turn_id,
            speaker_id,
            turn_duration,
            post_turn_silence,
            words: words
                .iter()
                .map(|(text, is_cut)| Word {
                    word_type: WordType::Normal,
                    text: text.to_string(),
                    start_sec: 0.0,
                    end_sec: 0.0,
                    is_cut: *is_cut,
                    is_muted: false,
                    source_onset_sample: None,
                    length_samples: 0,
                })
                .collect(),
            splices: vec![Splice {
                length_samples: total,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            }],
        };
        let (h, _) = encode_turn(&turn).unwrap();
        ImplicitTimelineTree::new()
            .insert_at(0, h, Arc::new(turn))
            .unwrap()
    }

    /// Append a turn to an existing tree.
    fn tx_append(
        tree: ImplicitTimelineTree<Turn>,
        turn_id: u64,
        speaker_id: Option<u64>,
        turn_duration: i64,
        post_turn_silence: i64,
        words: &[(&str, bool)],
    ) -> ImplicitTimelineTree<Turn> {
        let total = turn_duration + post_turn_silence;
        let turn = Turn {
            id: turn_id,
            speaker_id,
            turn_duration,
            post_turn_silence,
            words: words
                .iter()
                .map(|(text, is_cut)| Word {
                    word_type: WordType::Normal,
                    text: text.to_string(),
                    start_sec: 0.0,
                    end_sec: 0.0,
                    is_cut: *is_cut,
                    is_muted: false,
                    source_onset_sample: None,
                    length_samples: 0,
                })
                .collect(),
            splices: vec![Splice {
                length_samples: total,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Source {
                    source_start_sample: 0,
                },
            }],
        };
        let (h, _) = encode_turn(&turn).unwrap();
        let pos = tree.total_duration();
        tree.insert_at(pos, h, Arc::new(turn)).unwrap()
    }

    // T10: VTT structure — pinned output for a 2-turn, 2-speaker transcript.
    #[test]
    fn t10_vtt_structure() {
        // Turn 1: "Hello World" by Alice (1s speech, 1s silence) → [0, 1s)
        // Turn 2: "How are you" by Bob  (1s speech, 0s silence) → [2s, 3s)
        let tree = tx_append(
            make_tx_tree(
                1,
                Some(1),
                48_000,
                48_000,
                &[("Hello", false), ("World", false)],
            ),
            2,
            Some(2),
            48_000,
            0,
            &[("How", false), ("are", false), ("you", false)],
        );
        let mut speakers = BTreeMap::new();
        speakers.insert(1u64, "Alice".into());
        speakers.insert(2u64, "Bob".into());

        let vtt = format_transcript(
            &[(0, &tree)],
            &speakers,
            48_000,
            TranscriptFormat::Vtt,
            false,
        );

        const EXPECTED: &str = concat!(
            "WEBVTT\n",
            "\n",
            "00:00:00.000 --> 00:00:01.000\n",
            "<v Alice>Hello World\n",
            "\n",
            "00:00:02.000 --> 00:00:03.000\n",
            "<v Bob>How are you\n",
        );
        assert_eq!(vtt, EXPECTED, "T10: VTT structure pinned");
    }

    // T11: Markdown structure — pinned output for the same 2-turn transcript.
    #[test]
    fn t11_markdown_structure() {
        let tree = tx_append(
            make_tx_tree(
                1,
                Some(1),
                48_000,
                48_000,
                &[("Hello", false), ("World", false)],
            ),
            2,
            Some(2),
            48_000,
            0,
            &[("How", false), ("are", false), ("you", false)],
        );
        let mut speakers = BTreeMap::new();
        speakers.insert(1u64, "Alice".into());
        speakers.insert(2u64, "Bob".into());

        let md = format_transcript(
            &[(0, &tree)],
            &speakers,
            48_000,
            TranscriptFormat::Markdown,
            false,
        );

        const EXPECTED: &str = "**Alice:** Hello World\n\n**Bob:** How are you";
        assert_eq!(md, EXPECTED, "T11: Markdown structure pinned");
    }

    // T12: include_cut_words = false omits cut words; non-cut words and timestamps unaffected.
    #[test]
    fn t12_include_cut_words_false() {
        // "Hello", "um" (cut), "world"
        let tree = make_tx_tree(
            1,
            Some(1),
            48_000,
            0,
            &[("Hello", false), ("um", true), ("world", false)],
        );
        let speakers = BTreeMap::from([(1u64, "Alice".into())]);

        let vtt = format_transcript(
            &[(0, &tree)],
            &speakers,
            48_000,
            TranscriptFormat::Vtt,
            false,
        );
        assert!(!vtt.contains("um"), "T12 VTT: cut word must be absent");
        assert!(
            vtt.contains("Hello world"),
            "T12 VTT: non-cut words present"
        );

        let md = format_transcript(
            &[(0, &tree)],
            &speakers,
            48_000,
            TranscriptFormat::Markdown,
            false,
        );
        assert!(!md.contains("um"), "T12 Markdown: cut word absent");
        assert!(
            md.contains("Hello world"),
            "T12 Markdown: non-cut words present"
        );
    }

    // T13: include_cut_words = true keeps cut words.
    #[test]
    fn t13_include_cut_words_true() {
        let tree = make_tx_tree(
            1,
            Some(1),
            48_000,
            0,
            &[("Hello", false), ("um", true), ("world", false)],
        );
        let speakers = BTreeMap::from([(1u64, "Alice".into())]);

        for format in [TranscriptFormat::Vtt, TranscriptFormat::Markdown] {
            let out = format_transcript(&[(0, &tree)], &speakers, 48_000, format, true);
            assert!(
                out.contains("um"),
                "T13: cut word must appear when include_cut_words=true ({format:?})"
            );
        }
    }

    // T14: speaker_id == None renders as "[None]".
    #[test]
    fn t14_none_speaker() {
        let tree = make_tx_tree(1, None, 48_000, 0, &[("test", false)]);
        let speakers: BTreeMap<u64, String> = BTreeMap::new();

        let vtt = format_transcript(
            &[(0, &tree)],
            &speakers,
            48_000,
            TranscriptFormat::Vtt,
            false,
        );
        assert!(vtt.contains("<v [None]>"), "T14 VTT: [None] speaker tag");

        let md = format_transcript(
            &[(0, &tree)],
            &speakers,
            48_000,
            TranscriptFormat::Markdown,
            false,
        );
        assert!(md.contains("**[None]:**"), "T14 Markdown: [None] label");
    }

    // T15: Timestamp conversion — pure sample-to-timestamp arithmetic.
    #[test]
    fn t15_timestamp_conversion() {
        assert_eq!(samples_to_timestamp(0, 48_000), "00:00:00.000", "T15: 0");
        assert_eq!(samples_to_timestamp(48, 48_000), "00:00:00.001", "T15: 1ms");
        assert_eq!(
            samples_to_timestamp(48_000, 48_000),
            "00:00:01.000",
            "T15: 1s"
        );
        assert_eq!(
            samples_to_timestamp(2_880_000, 48_000),
            "00:01:00.000",
            "T15: 1min"
        );
        assert_eq!(
            samples_to_timestamp(172_800_000, 48_000),
            "01:00:00.000",
            "T15: 1hr"
        );
        // Floor (no rounding up): 47 samples at 48000 Hz = 0.9791… ms → 0ms
        assert_eq!(
            samples_to_timestamp(47, 48_000),
            "00:00:00.000",
            "T15: floor"
        );
    }

    // T16: Extension routing for transcript formats.
    #[test]
    fn t16_transcript_format_for_extension() {
        assert_eq!(
            transcript_format_for(Path::new("t.vtt")),
            Some(TranscriptFormat::Vtt),
            "T16: .vtt"
        );
        assert_eq!(
            transcript_format_for(Path::new("t.md")),
            Some(TranscriptFormat::Markdown),
            "T16: .md"
        );
        assert_eq!(
            transcript_format_for(Path::new("t.markdown")),
            Some(TranscriptFormat::Markdown),
            "T16: .markdown"
        );
        assert_eq!(
            transcript_format_for(Path::new("t.txt")),
            None,
            "T16: .txt → None"
        );
        assert_eq!(
            transcript_format_for(Path::new("no_extension")),
            None,
            "T16: no extension → None"
        );
        // Extension matching is case-insensitive.
        assert_eq!(
            transcript_format_for(Path::new("t.VTT")),
            Some(TranscriptFormat::Vtt),
            "T16: .VTT"
        );
        assert_eq!(
            transcript_format_for(Path::new("t.Markdown")),
            Some(TranscriptFormat::Markdown),
            "T16: .Markdown"
        );
    }

    // T17: Extension wins — transcript_format_for(".md") returns Markdown, and format_transcript
    // produces Markdown when called with it.
    #[test]
    fn t17_extension_overrides_param() {
        let tree = make_tx_tree(1, Some(1), 48_000, 0, &[("Hello", false)]);
        let speakers = BTreeMap::from([(1u64, "Alice".into())]);

        let format = transcript_format_for(Path::new("output.md")).unwrap();
        assert_eq!(format, TranscriptFormat::Markdown, "T17: .md → Markdown");

        let out = format_transcript(&[(0, &tree)], &speakers, 48_000, format, false);
        assert!(out.starts_with("**"), "T17: output is Markdown, not VTT");
        assert!(!out.starts_with("WEBVTT"), "T17: not VTT");
    }

    // T18: Empty transcript (no turns) → valid header-only output, no panic.
    #[test]
    fn t18_empty_transcript() {
        let empty_tree: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        let speakers: BTreeMap<u64, String> = BTreeMap::new();

        let vtt = format_transcript(
            &[(0, &empty_tree)],
            &speakers,
            48_000,
            TranscriptFormat::Vtt,
            false,
        );
        assert_eq!(vtt, "WEBVTT\n", "T18 VTT: header-only for empty transcript");

        let md = format_transcript(
            &[(0, &empty_tree)],
            &speakers,
            48_000,
            TranscriptFormat::Markdown,
            false,
        );
        assert_eq!(md, "", "T18 Markdown: empty string for empty transcript");
    }
}
