//! Journal read/write helpers: snapshot-row lookup, delta-row range scan,
//! metadata-row lookup, and row append.

use rusqlite::{Connection, OptionalExtension};

use crate::project::command_id::CommandId;
use crate::project::hash::Hash;

/// A `type = 1` snapshot journal row.
#[derive(Debug)]
pub(crate) struct SnapshotRow {
    /// Row id.
    pub id: i64,
    /// Hash of the snapshot blob this row points to.
    pub hash: Hash,
    /// Command-type enum code that produced the row.
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub command_id: i64,
    /// Creation time, POSIX seconds UTC.
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub applied_at: i64,
}

/// A `type = 0` delta journal row.
#[derive(Debug)]
pub(crate) struct DeltaRow {
    /// Row id.
    pub id: i64,
    /// Version-prefixed postcard `Vec<Delta>` payload.
    pub payload: Vec<u8>,
    /// Command-type enum code that produced the row.
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub command_id: i64,
    /// Creation time, POSIX seconds UTC.
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub applied_at: i64,
}

/// A `type = -1` metadata journal row. Same shape as [`SnapshotRow`] (both are
/// 16-byte hash-pointer rows) but kept distinct: it points at a metadata blob,
/// and the history view may treat the two row types differently.
#[derive(Debug)]
pub(crate) struct MetaRow {
    /// Row id. Read by the history-view feature (Step 12+).
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub id: i64,
    /// Hash of the metadata blob this row points to.
    pub hash: Hash,
    /// Raw command-type code; map via `CommandId::from_code` at a higher layer.
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub command_id: i64,
    /// Creation time, POSIX seconds UTC.
    #[allow(dead_code)] // Step 12+: history view will read this.
    pub applied_at: i64,
}

/// Errors from journal helpers.
#[derive(Debug)]
pub(crate) enum JournalError {
    /// Underlying SQLite error.
    Sqlite(rusqlite::Error),
    /// A hash-pointer row's payload was not exactly 16 bytes.
    MalformedHashPayload {
        /// The row id.
        id: i64,
        /// Actual payload length.
        len: usize,
    },
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            JournalError::MalformedHashPayload { id, len } => {
                write!(f, "journal row {id} has a {len}-byte payload (expected 16)")
            }
        }
    }
}

impl std::error::Error for JournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            JournalError::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for JournalError {
    fn from(e: rusqlite::Error) -> Self {
        JournalError::Sqlite(e)
    }
}

/// Most recent `type = 1` row with `id <= as_of` (highest such id), or the
/// absolute latest when `as_of` is `None`. `None` result ⇒ no snapshot in range.
pub(crate) fn latest_snapshot(
    conn: &Connection,
    as_of: Option<i64>,
) -> Result<Option<SnapshotRow>, JournalError> {
    let row: Option<(i64, Vec<u8>, i64, i64)> = conn
        .query_row(
            "SELECT id, payload, command_id, applied_at FROM journal \
             WHERE type = 1 AND (?1 IS NULL OR id <= ?1) ORDER BY id DESC LIMIT 1",
            rusqlite::params![as_of],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    match row {
        None => Ok(None),
        Some((id, payload, command_id, applied_at)) => {
            let len = payload.len();
            let bytes: [u8; 16] = payload
                .try_into()
                .map_err(|_| JournalError::MalformedHashPayload { id, len })?;
            Ok(Some(SnapshotRow {
                id,
                hash: Hash(bytes),
                command_id,
                applied_at,
            }))
        }
    }
}

/// All `type = 0` rows with `snapshot_id < id <= until` (or just `id > snapshot_id`
/// when `until` is `None`), in ascending `id` order.
pub(crate) fn deltas_after(
    conn: &Connection,
    snapshot_id: i64,
    until: Option<i64>,
) -> Result<Vec<DeltaRow>, JournalError> {
    let mut stmt = conn.prepare(
        "SELECT id, payload, command_id, applied_at FROM journal \
         WHERE type = 0 AND id > ?1 AND (?2 IS NULL OR id <= ?2) ORDER BY id ASC",
    )?;

    let rows = stmt.query_map(rusqlite::params![snapshot_id, until], |r| {
        Ok(DeltaRow {
            id: r.get(0)?,
            payload: r.get(1)?,
            command_id: r.get(2)?,
            applied_at: r.get(3)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

/// Most recent `type = -1` row with `id <= as_of` (highest such id), or the
/// absolute latest when `as_of` is `None`. `None` result ⇒ no metadata row in range.
pub(crate) fn latest_metadata(
    conn: &Connection,
    as_of: Option<i64>,
) -> Result<Option<MetaRow>, JournalError> {
    let row: Option<(i64, Vec<u8>, i64, i64)> = conn
        .query_row(
            "SELECT id, payload, command_id, applied_at FROM journal \
             WHERE type = -1 AND (?1 IS NULL OR id <= ?1) ORDER BY id DESC LIMIT 1",
            rusqlite::params![as_of],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;

    match row {
        None => Ok(None),
        Some((id, payload, command_id, applied_at)) => {
            let len = payload.len();
            let bytes: [u8; 16] = payload
                .try_into()
                .map_err(|_| JournalError::MalformedHashPayload { id, len })?;
            Ok(Some(MetaRow {
                id,
                hash: Hash(bytes),
                command_id,
                applied_at,
            }))
        }
    }
}

/// Append one journal row, returning its assigned `id` (`AUTOINCREMENT`).
/// Private: all callers go through the typed wrappers so the `type` code and
/// payload shape are never mismatched at a call site.
fn append_row(
    conn: &Connection,
    row_type: i64,
    command_id: CommandId,
    payload: &[u8],
    applied_at: i64,
) -> Result<i64, JournalError> {
    conn.execute(
        "INSERT INTO journal (type, payload, command_id, applied_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![row_type, payload, command_id.code(), applied_at],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Append a `type = 0` delta-batch row. `payload` is the version-prefixed
/// `Vec<Delta>` bytes from `delta::encode_delta_batch`.
pub(crate) fn append_delta_batch(
    conn: &Connection,
    command_id: CommandId,
    payload: &[u8],
    applied_at: i64,
) -> Result<i64, JournalError> {
    append_row(conn, 0, command_id, payload, applied_at)
}

/// Append a `type = 1` snapshot row pointing at the snapshot blob `hash`.
/// The payload is the bare 16-byte hash (no extra version byte).
pub(crate) fn append_snapshot(
    conn: &Connection,
    command_id: CommandId,
    hash: &Hash,
    applied_at: i64,
) -> Result<i64, JournalError> {
    append_row(conn, 1, command_id, &hash.0[..], applied_at)
}

/// Append a `type = -1` metadata row pointing at the metadata blob `hash`.
/// Payload is the bare 16-byte hash, same as `append_snapshot`.
pub(crate) fn append_metadata(
    conn: &Connection,
    command_id: CommandId,
    hash: &Hash,
    applied_at: i64,
) -> Result<i64, JournalError> {
    append_row(conn, -1, command_id, &hash.0[..], applied_at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::project::delta::{encode_delta_batch, Delta, Location};
    use tempfile::tempdir;

    #[test]
    fn journal_error_display_messages() {
        let sqlite_err = JournalError::Sqlite(rusqlite::Error::QueryReturnedNoRows).to_string();
        assert!(
            sqlite_err.contains("sqlite"),
            "Sqlite display: {sqlite_err}"
        );

        let malformed = JournalError::MalformedHashPayload { id: 42, len: 7 }.to_string();
        assert!(
            malformed.contains("42") && malformed.contains('7'),
            "MalformedHashPayload display: {malformed}"
        );
    }

    #[test]
    fn journal_error_source_impl() {
        use std::error::Error;

        assert!(
            JournalError::Sqlite(rusqlite::Error::QueryReturnedNoRows)
                .source()
                .is_some(),
            "Sqlite variant must chain the inner error"
        );
        assert!(
            JournalError::MalformedHashPayload { id: 1, len: 15 }
                .source()
                .is_none(),
            "MalformedHashPayload has no inner error to chain"
        );
    }

    fn open_tmp_db() -> (tempfile::TempDir, Db) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = Db::create(&path).unwrap();
        (dir, db)
    }

    fn insert_journal_row(
        conn: &Connection,
        row_type: i64,
        payload: &[u8],
        command_id: i64,
        applied_at: i64,
    ) -> i64 {
        conn.execute(
            "INSERT INTO journal (type, payload, command_id, applied_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![row_type, payload, command_id, applied_at],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn snap_payload(byte: u8) -> [u8; 16] {
        let mut arr = [0u8; 16];
        arr[0] = byte;
        arr
    }

    // J1
    #[test]
    fn latest_snapshot_none_on_empty_journal() {
        let (_dir, db) = open_tmp_db();
        let result = latest_snapshot(db.conn(), None).unwrap();
        assert!(result.is_none());
    }

    // J2
    #[test]
    fn latest_snapshot_returns_highest_id() {
        let (_dir, db) = open_tmp_db();
        let h1 = snap_payload(0x01);
        let h2 = snap_payload(0x02);
        insert_journal_row(db.conn(), 1, &h1, 10, 1000);
        insert_journal_row(db.conn(), 0, &[0x01, 0x00], 11, 1001);
        let id3 = insert_journal_row(db.conn(), 1, &h2, 12, 1002);

        let row = latest_snapshot(db.conn(), None).unwrap().unwrap();
        assert_eq!(row.id, id3);
        assert_eq!(row.hash.0, h2);
    }

    // J3
    #[test]
    fn latest_snapshot_rejects_non_16_byte_payload() {
        let (_dir, db) = open_tmp_db();
        let bad_payload = [0u8; 15];
        let row_id = insert_journal_row(db.conn(), 1, &bad_payload, 10, 1000);

        let err = latest_snapshot(db.conn(), None).unwrap_err();
        assert!(
            matches!(err, JournalError::MalformedHashPayload { id, len: 15 } if id == row_id),
            "expected MalformedHashPayload {{ id={row_id}, len=15 }}, got: {err:?}"
        );
    }

    // J4
    #[test]
    fn latest_snapshot_as_of_bounds() {
        let (_dir, db) = open_tmp_db();
        let h1 = snap_payload(0x01);
        let h2 = snap_payload(0x02);
        let s1 = insert_journal_row(db.conn(), 1, &h1, 10, 1000);
        let s2 = insert_journal_row(db.conn(), 1, &h2, 11, 1001);

        let r = latest_snapshot(db.conn(), Some(s2 - 1)).unwrap().unwrap();
        assert_eq!(r.id, s1, "Some(s2-1) should return s1");

        let r = latest_snapshot(db.conn(), Some(s2)).unwrap().unwrap();
        assert_eq!(r.id, s2, "Some(s2) should return s2");

        let r = latest_snapshot(db.conn(), Some(s1 - 1)).unwrap();
        assert!(r.is_none(), "Some(s1-1) should return None");

        let r = latest_snapshot(db.conn(), None).unwrap().unwrap();
        assert_eq!(r.id, s2, "None should return absolute latest (s2)");
    }

    // J5
    #[test]
    fn latest_snapshot_surfaces_command_id_and_applied_at() {
        let (_dir, db) = open_tmp_db();
        let h = snap_payload(0x42);
        insert_journal_row(db.conn(), 1, &h, 99, 12345);

        let row = latest_snapshot(db.conn(), None).unwrap().unwrap();
        assert_eq!(row.command_id, 99);
        assert_eq!(row.applied_at, 12345);
    }

    // J6
    #[test]
    fn deltas_after_returns_ascending_subset() {
        let (_dir, db) = open_tmp_db();
        let h = snap_payload(0x01);

        // Insert an early delta BEFORE the snapshot (lower id)
        let d_early = insert_journal_row(db.conn(), 0, &[0x01, 0x00], 10, 1000);
        // Insert the snapshot (id S > d_early by AUTOINCREMENT)
        let s = insert_journal_row(db.conn(), 1, &h, 11, 1001);
        assert!(d_early < s, "d_early should have a lower id than snapshot");
        // Insert three deltas after snapshot
        let d1 = insert_journal_row(db.conn(), 0, &[0x01, 0x01], 12, 1002);
        let d2 = insert_journal_row(db.conn(), 0, &[0x01, 0x02], 13, 1003);
        let d3 = insert_journal_row(db.conn(), 0, &[0x01, 0x03], 14, 1004);
        // Insert a type=-1 (metadata) row
        insert_journal_row(db.conn(), -1, &h, 15, 1005);

        let rows = deltas_after(db.conn(), s, None).unwrap();
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![d1, d2, d3],
            "should return exactly the three post-snapshot type=0 rows in ascending order"
        );
        // Verify command_id / applied_at round-trip
        assert_eq!(rows[0].command_id, 12);
        assert_eq!(rows[0].applied_at, 1002);
        assert_eq!(rows[1].command_id, 13);
        assert_eq!(rows[1].applied_at, 1003);
        assert_eq!(rows[2].command_id, 14);
        assert_eq!(rows[2].applied_at, 1004);
    }

    // J7
    #[test]
    fn deltas_after_until_bounds() {
        let (_dir, db) = open_tmp_db();
        let h = snap_payload(0x01);
        let s = insert_journal_row(db.conn(), 1, &h, 10, 1000);
        let d1 = insert_journal_row(db.conn(), 0, &[0x01, 0x01], 11, 1001);
        let d2 = insert_journal_row(db.conn(), 0, &[0x01, 0x02], 12, 1002);
        let d3 = insert_journal_row(db.conn(), 0, &[0x01, 0x03], 13, 1003);

        let ids = |rows: Vec<DeltaRow>| rows.into_iter().map(|r| r.id).collect::<Vec<_>>();

        assert_eq!(
            ids(deltas_after(db.conn(), s, Some(d2)).unwrap()),
            vec![d1, d2],
            "Some(d2) should include d1 and d2 (inclusive upper bound)"
        );
        assert_eq!(
            ids(deltas_after(db.conn(), s, Some(d3)).unwrap()),
            vec![d1, d2, d3],
            "Some(d3) should include all three"
        );
        assert_eq!(
            ids(deltas_after(db.conn(), s, None).unwrap()),
            vec![d1, d2, d3],
            "None should include all three"
        );

        // Unsatisfiable predicate: id > s AND id <= s-1 → empty
        let rows = deltas_after(db.conn(), s, Some(s - 1)).unwrap();
        assert!(
            rows.is_empty(),
            "id > {s} AND id <= {} is unsatisfiable; expected empty",
            s - 1
        );
    }

    // J8
    #[test]
    fn deltas_after_empty_when_none_follow() {
        let (_dir, db) = open_tmp_db();
        let h = snap_payload(0x01);
        let s = insert_journal_row(db.conn(), 1, &h, 10, 1000);
        let rows = deltas_after(db.conn(), s, None).unwrap();
        assert!(rows.is_empty());
    }

    // W1
    #[test]
    fn append_snapshot_round_trips_via_latest_snapshot() {
        let (_dir, db) = open_tmp_db();
        let h = Hash(snap_payload(0xAB));
        let id = append_snapshot(db.conn(), CommandId::Unknown, &h, 1000).unwrap();
        assert!(id > 0);
        let row = latest_snapshot(db.conn(), None).unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.hash.0, h.0);
        assert_eq!(row.command_id, CommandId::Unknown.code());
        assert_eq!(row.applied_at, 1000);
    }

    // W2
    #[test]
    fn append_delta_batch_round_trips_via_deltas_after() {
        let (_dir, db) = open_tmp_db();
        let h = Hash(snap_payload(0x01));
        let snap_id = append_snapshot(db.conn(), CommandId::Unknown, &h, 999).unwrap();

        let batch = vec![Delta::insert_after(1, Location::Start, Hash([0xBB; 16]))];
        let payload = encode_delta_batch(&batch).unwrap();
        let delta_id = append_delta_batch(db.conn(), CommandId::Cut, &payload, 1001).unwrap();
        assert!(delta_id > snap_id);

        let rows = deltas_after(db.conn(), snap_id, None).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, delta_id);
        assert_eq!(rows[0].payload, payload);
        assert_eq!(rows[0].command_id, CommandId::Cut.code());
        assert_eq!(rows[0].applied_at, 1001);
    }

    // W3
    #[test]
    fn append_metadata_round_trips_via_latest_metadata() {
        let (_dir, db) = open_tmp_db();
        let h = Hash(snap_payload(0xCC));
        let id = append_metadata(db.conn(), CommandId::Unknown, &h, 2000).unwrap();
        let row = latest_metadata(db.conn(), None).unwrap().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.hash.0, h.0);
        assert_eq!(row.command_id, CommandId::Unknown.code());
        assert_eq!(row.applied_at, 2000);
    }

    // W4
    #[test]
    fn append_returns_increasing_ids() {
        let (_dir, db) = open_tmp_db();
        let h = Hash([0u8; 16]);
        let id1 = append_snapshot(db.conn(), CommandId::Unknown, &h, 1).unwrap();
        let id2 = append_delta_batch(db.conn(), CommandId::Unknown, &[0x01, 0x00], 2).unwrap();
        let id3 = append_metadata(db.conn(), CommandId::Unknown, &h, 3).unwrap();
        assert!(id1 < id2 && id2 < id3, "ids should be strictly increasing");
    }

    // W5
    #[test]
    fn latest_metadata_none_on_empty_journal() {
        let (_dir, db) = open_tmp_db();
        assert!(latest_metadata(db.conn(), None).unwrap().is_none());
    }

    // W6
    #[test]
    fn latest_metadata_returns_highest_id() {
        let (_dir, db) = open_tmp_db();
        let h1 = Hash(snap_payload(0x01));
        let h2 = Hash(snap_payload(0x02));
        let m1 = append_metadata(db.conn(), CommandId::Unknown, &h1, 100).unwrap();
        // Intervening delta row
        append_delta_batch(db.conn(), CommandId::Unknown, &[0x01, 0x00], 101).unwrap();
        let m2 = append_metadata(db.conn(), CommandId::Unknown, &h2, 102).unwrap();
        assert!(m1 < m2);

        let row = latest_metadata(db.conn(), None).unwrap().unwrap();
        assert_eq!(row.id, m2);
        assert_eq!(row.hash.0, h2.0);
    }

    // W6b
    #[test]
    fn latest_metadata_as_of_bounds() {
        let (_dir, db) = open_tmp_db();
        let h1 = Hash(snap_payload(0x01));
        let h2 = Hash(snap_payload(0x02));
        let m1 = append_metadata(db.conn(), CommandId::Unknown, &h1, 100).unwrap();
        let m2 = append_metadata(db.conn(), CommandId::Unknown, &h2, 101).unwrap();

        let r = latest_metadata(db.conn(), Some(m2 - 1)).unwrap().unwrap();
        assert_eq!(r.id, m1, "Some(m2-1) should return m1");

        let r = latest_metadata(db.conn(), Some(m2)).unwrap().unwrap();
        assert_eq!(r.id, m2, "Some(m2) should return m2");

        let r = latest_metadata(db.conn(), Some(m1 - 1)).unwrap();
        assert!(r.is_none(), "Some(m1-1) should return None");

        let r = latest_metadata(db.conn(), None).unwrap().unwrap();
        assert_eq!(r.id, m2, "None should return absolute latest (m2)");
    }

    // W7
    #[test]
    fn latest_metadata_rejects_non_16_byte_payload() {
        let (_dir, db) = open_tmp_db();
        let bad_payload = [0u8; 15];
        let row_id = insert_journal_row(db.conn(), -1, &bad_payload, 0, 1000);

        let err = latest_metadata(db.conn(), None).unwrap_err();
        assert!(
            matches!(err, JournalError::MalformedHashPayload { id, len: 15 } if id == row_id),
            "expected MalformedHashPayload {{ id={row_id}, len=15 }}, got: {err:?}"
        );
    }

    // W8
    #[test]
    fn append_rolls_back_on_transaction_error() {
        let (_dir, mut db) = open_tmp_db();
        let h = Hash(snap_payload(0x99));
        let _: anyhow::Result<()> = db.with_transaction(|tx| {
            append_snapshot(tx, CommandId::Unknown, &h, 5000)?;
            anyhow::bail!("intentional failure")
        });
        assert!(latest_snapshot(db.conn(), None).unwrap().is_none());
    }

    // W9
    #[test]
    fn malformed_hash_payload_display_is_generic() {
        let msg = JournalError::MalformedHashPayload { id: 7, len: 15 }.to_string();
        assert!(msg.contains('7'), "should contain the row id: {msg}");
        assert!(
            msg.contains("16") || msg.contains("15"),
            "should mention the expected or actual length: {msg}"
        );
        assert!(
            !msg.contains("snapshot"),
            "should not hardcode 'snapshot': {msg}"
        );
    }
}
