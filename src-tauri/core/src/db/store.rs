//! Content-addressed blob store: thin INSERT/SELECT over the `store` table.
//!
//! Callers serialize their object via `hash.rs` / per-kind store helpers,
//! receive `(Hash, Vec<u8>)`, and hand both to `put`. `get` is the inverse
//! — it returns the tagged bytes after verifying the on-disk payload still
//! hashes to the requested key (bit-rot detection).

use rusqlite::{Connection, OptionalExtension};

use crate::project::hash::{hash_tagged, Hash};

/// Errors returned by blob-store operations.
#[derive(Debug)]
pub enum StoreError {
    /// No row exists in `store` with the requested hash.
    NotFound(Hash),
    /// A row exists, but its payload no longer hashes to the lookup key
    /// (on-disk corruption — bit-flip, partial write, schema tampering).
    HashMismatch {
        /// Hash the caller asked for (and used as the row's PRIMARY KEY).
        expected: Hash,
        /// Hash actually computed from the fetched payload bytes.
        computed: Hash,
    },
    /// Underlying SQLite I/O or query error.
    Sqlite(rusqlite::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotFound(h) => write!(f, "blob not found: {h:?}"),
            StoreError::HashMismatch { expected, computed } => write!(
                f,
                "on-disk corruption: expected {expected:?}, computed {computed:?}"
            ),
            StoreError::Sqlite(e) => write!(f, "sqlite error: {e}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Sqlite(e) => Some(e),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

/// Insert a content-addressed blob, idempotent on duplicate.
///
/// `bytes` must be the full tag ++ postcard blob produced by
/// `encode_tagged` (or a per-kind helper that wraps it); `hash` must be the
/// 128-bit BLAKE3 of those bytes. A `debug_assert!` checks the pairing in
/// debug builds; release builds trust the caller and rely on `get`'s
/// re-hash to surface any mismatch on read.
///
/// Returns `Ok(true)` when a new row was written, `Ok(false)` when a row
/// with this hash was already present (the canonical dedup signal).
pub(crate) fn put(conn: &Connection, hash: &Hash, bytes: &[u8]) -> Result<bool, StoreError> {
    debug_assert_eq!(
        hash_tagged(bytes),
        *hash,
        "store::put called with hash that doesn't match bytes",
    );
    let affected = conn.execute(
        "INSERT OR IGNORE INTO store (hash, payload) VALUES (?1, ?2)",
        (&hash.0[..], bytes),
    )?;
    Ok(affected == 1)
}

/// Fetch a content-addressed blob, verifying its on-disk integrity.
///
/// Re-hashes the fetched payload and returns
/// [`StoreError::HashMismatch`] if the recomputed hash differs from
/// `hash` (bit-rot detection — postcard-valid corruption that
/// `hash.rs` cannot see). Returns [`StoreError::NotFound`] if no row
/// exists for `hash`.
///
/// The returned `Vec<u8>` is the full tagged blob, suitable for the
/// kind-specific loader (`decode_turn`, `decode_label`, …) without further
/// processing.
pub(crate) fn get(conn: &Connection, hash: &Hash) -> Result<Vec<u8>, StoreError> {
    let payload: Vec<u8> = conn
        .query_row(
            "SELECT payload FROM store WHERE hash = ?1",
            (&hash.0[..],),
            |r| r.get(0),
        )
        .optional()?
        .ok_or(StoreError::NotFound(*hash))?;
    let computed = hash_tagged(&payload);
    if computed != *hash {
        return Err(StoreError::HashMismatch {
            expected: *hash,
            computed,
        });
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::turn::{encode_turn, Splice, SpliceKind, Turn, Word, WordType};

    fn open_tmp_db() -> (tempfile::TempDir, super::super::Db) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.vocalboard");
        let db = super::super::Db::create(&path).unwrap();
        (dir, db)
    }

    fn sample_blob_with_id(id: u64) -> (Hash, Vec<u8>) {
        encode_turn(&Turn {
            id,
            speaker_id: Some(1),
            turn_duration: 44100,
            post_turn_silence: 8820,
            words: vec![Word {
                word_type: WordType::Normal,
                text: "hello".into(),
                start_sec: 0.1,
                end_sec: 0.5,
                is_cut: false,
                is_muted: false,
                turn_offset_sample: 4410,
                length_samples: 17640,
            }],
            splices: vec![Splice {
                length_samples: 52920,
                fade_in_samples: 0,
                fade_out_samples: 0,
                kind: SpliceKind::Silence,
            }],
        })
        .unwrap()
    }

    fn sample_blob() -> (Hash, Vec<u8>) {
        sample_blob_with_id(1)
    }

    #[test]
    fn store_error_display_messages() {
        let h = Hash([0u8; 16]);
        let not_found = StoreError::NotFound(h).to_string();
        assert!(
            not_found.contains("not found"),
            "NotFound display: {not_found}"
        );

        let h2 = Hash([1u8; 16]);
        let mismatch = StoreError::HashMismatch {
            expected: h,
            computed: h2,
        }
        .to_string();
        assert!(
            mismatch.contains("corruption"),
            "HashMismatch display: {mismatch}"
        );

        let sqlite_err = StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows).to_string();
        assert!(
            sqlite_err.contains("sqlite"),
            "Sqlite display: {sqlite_err}"
        );
    }

    #[test]
    fn store_error_source_impl() {
        use std::error::Error;

        let h = Hash([0u8; 16]);
        assert!(StoreError::NotFound(h).source().is_none());
        assert!(StoreError::HashMismatch {
            expected: h,
            computed: Hash([1u8; 16])
        }
        .source()
        .is_none());
        assert!(
            StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)
                .source()
                .is_some(),
            "Sqlite variant must chain the inner error"
        );
    }

    #[test]
    fn put_get_round_trip() {
        let (_dir, db) = open_tmp_db();
        let (h, b) = sample_blob();
        put(db.conn(), &h, &b).unwrap();
        let got = get(db.conn(), &h).unwrap();
        assert_eq!(got, b);
    }

    #[test]
    fn put_returns_true_on_first_insert() {
        let (_dir, db) = open_tmp_db();
        let (h, b) = sample_blob();
        assert!(put(db.conn(), &h, &b).unwrap());
    }

    #[test]
    fn put_returns_false_on_duplicate() {
        let (_dir, db) = open_tmp_db();
        let (h, b) = sample_blob();
        assert!(put(db.conn(), &h, &b).unwrap());
        assert!(!put(db.conn(), &h, &b).unwrap());
    }

    #[test]
    fn put_dedup_keeps_one_row() {
        let (_dir, db) = open_tmp_db();
        let (h, b) = sample_blob();
        for _ in 0..5 {
            put(db.conn(), &h, &b).unwrap();
        }
        let count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM store WHERE hash = ?1",
                (&h.0[..],),
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn distinct_blobs_coexist() {
        let (_dir, db) = open_tmp_db();
        let (h1, b1) = sample_blob_with_id(1);
        let (h2, b2) = sample_blob_with_id(2);
        assert_ne!(h1, h2);
        put(db.conn(), &h1, &b1).unwrap();
        put(db.conn(), &h2, &b2).unwrap();
        assert_eq!(get(db.conn(), &h1).unwrap(), b1);
        assert_eq!(get(db.conn(), &h2).unwrap(), b2);
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM store", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn get_missing_returns_not_found() {
        let (_dir, db) = open_tmp_db();
        let h = Hash([0u8; 16]);
        let err = get(db.conn(), &h).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(got) if got.0 == [0u8; 16]));
    }

    #[test]
    fn get_detects_payload_corruption() {
        let (_dir, db) = open_tmp_db();
        let (h, b) = sample_blob();
        put(db.conn(), &h, &b).unwrap();
        let mut corrupted = b.clone();
        corrupted[0] ^= 0xFF;
        db.conn()
            .execute(
                "UPDATE store SET payload = ?1 WHERE hash = ?2",
                (&corrupted[..], &h.0[..]),
            )
            .unwrap();
        let err = get(db.conn(), &h).unwrap_err();
        assert!(matches!(err, StoreError::HashMismatch { .. }));
    }

    #[test]
    fn get_detects_truncation() {
        let (_dir, db) = open_tmp_db();
        let (h, b) = sample_blob();
        put(db.conn(), &h, &b).unwrap();
        let truncated = &b[..b.len() - 1];
        db.conn()
            .execute(
                "UPDATE store SET payload = ?1 WHERE hash = ?2",
                (truncated, &h.0[..]),
            )
            .unwrap();
        let err = get(db.conn(), &h).unwrap_err();
        assert!(matches!(err, StoreError::HashMismatch { .. }));
    }

    #[test]
    fn put_then_get_works_inside_transaction() {
        let (_dir, mut db) = open_tmp_db();
        let (h, b) = sample_blob();
        db.with_transaction(|tx| -> anyhow::Result<()> {
            put(tx, &h, &b)?;
            let read = get(tx, &h)?;
            assert_eq!(read, b);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn put_rolls_back_on_transaction_error() {
        let (_dir, mut db) = open_tmp_db();
        let (h, b) = sample_blob();
        let _: anyhow::Result<()> = db.with_transaction(|tx| {
            put(tx, &h, &b)?;
            anyhow::bail!("intentional failure")
        });
        let err = get(db.conn(), &h).unwrap_err();
        assert!(matches!(err, StoreError::NotFound(_)));
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "doesn't match")]
    fn put_hash_mismatch_debug_assert() {
        let (_dir, db) = open_tmp_db();
        let (_, b) = sample_blob();
        let _ = put(db.conn(), &Hash([0u8; 16]), &b);
    }

    #[test]
    fn empty_payload_round_trips() {
        let (_dir, db) = open_tmp_db();
        let h = hash_tagged(&[]);
        put(db.conn(), &h, &[]).unwrap();
        let got = get(db.conn(), &h).unwrap();
        assert!(got.is_empty());
    }
}
