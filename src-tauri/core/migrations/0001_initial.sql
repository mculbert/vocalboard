-- Phase 1 initial schema (user_version = 1).
-- Connection-level pragmas (WAL, foreign_keys) are set by Db::open, not here.

-- ── Project metadata ─────────────────────────────────────────────────────────

CREATE TABLE project (
    id               INTEGER PRIMARY KEY CHECK (id = 1), -- singleton row
    schema_version   INTEGER NOT NULL DEFAULT 1,
    min_app_version  TEXT    NOT NULL DEFAULT '0.1.1',   -- semver; older apps refuse this file
    sample_rate      INTEGER NOT NULL DEFAULT 48000,     -- locked at creation
    -- Monotonic ID counters: the NEXT value to assign. Persisted so that IDs
    -- stay unique across sessions. Track 0 is reserved for the labels track,
    -- so next_track_id starts at 1.
    next_track_id    INTEGER NOT NULL DEFAULT 1,
    next_speaker_id  INTEGER NOT NULL DEFAULT 1,
    next_turn_id     INTEGER NOT NULL DEFAULT 1,         -- persistent turn IDs
    next_label_id    INTEGER NOT NULL DEFAULT 1,         -- persistent label IDs (track 0)
    created_at       TEXT    NOT NULL,                   -- ISO 8601
    updated_at       TEXT    NOT NULL
);

-- ── Content-addressed blob store ─────────────────────────────────────────────
-- Git-style object store. One row per unique blob, keyed by the 128-bit BLAKE3
-- hash of its postcard-serialized payload. Turns, the global metadata object, and
-- large binaries (room tone PCM, speaker embeddings) all live here. A given
-- byte-identical payload is stored exactly once, no matter how many snapshots or
-- journal entries reference it — so an unchanged turn costs nothing across snapshots.

CREATE TABLE store (
    hash     BLOB PRIMARY KEY,   -- 16 bytes: BLAKE3 digest truncated to 128 bits
    payload  BLOB NOT NULL       -- format-tagged, postcard-serialized object
);

-- ── Edit journal ─────────────────────────────────────────────────────────────
-- Append-only. Each row is one of: a batch of timeline deltas, a full timeline
-- snapshot, or a non-timeline (metadata) change. Replayed in id order on open,
-- starting from the most recent snapshot.

CREATE TABLE journal (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- -1 = non-timeline data: payload = 16-byte hash of a global metadata blob
    --  0 = timeline delta batch: payload = inline postcard Vec<Delta>
    --  1 = full timeline snapshot: payload = 16-byte hash of a snapshot blob
    type         INTEGER NOT NULL CHECK (type IN (-1, 0, 1)),
    payload      BLOB    NOT NULL,
    command_id   INTEGER NOT NULL,   -- enum CODE for the command type that produced this
                                     -- row (e.g. CutWords, AlignTracks); NOT a counter
    applied_at   INTEGER NOT NULL    -- POSIX time in seconds, UTC
);

-- Fast "most recent snapshot" lookup and "deltas after snapshot" range scan.
CREATE INDEX journal_type_idx ON journal(type, id DESC);
