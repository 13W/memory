//! Free-space accounting and reclamation for the SQLite stores (`X-012`).
//!
//! SQLite never returns a deleted page to the filesystem on its own. With
//! `auto_vacuum = NONE` — the default this store shipped with — a swept
//! generation frees pages *inside* the file and the file itself keeps its
//! high-water mark forever. Measured on the owner's store before this module
//! existed: `page_count` 14 875 928 against `freelist_count` 9 880 851 at a
//! 4096-byte page, i.e. **57 GB on disk of which 66 % was holes**, over roughly
//! 19.5 GB of live data. Nothing in the product measured that, so nothing said
//! it.
//!
//! # Two reclamation paths, and why the split is not symmetry for its own sake
//!
//! A full `VACUUM` rewrites the entire database. It needs exclusive access and
//! free disk for a second copy of the live data, and on a store this size it
//! runs for many minutes — which is exactly why it belongs to an operator
//! command and never to a background worker: a daemon that froze the store for
//! twenty minutes while holding `store.lock` would be a worse defect than the
//! bloat it was clearing.
//!
//! `PRAGMA incremental_vacuum(N)`, by contrast, moves at most `N` pages and
//! returns. It needs `auto_vacuum = INCREMENTAL`, which a database can only
//! adopt while it is still empty *or* during a full `VACUUM` — so the operator
//! pass is also the conversion, and everything after it is cheap.
//!
//! # One measured fact this module is built on
//!
//! `auto_vacuum` must be set **before** `journal_mode = WAL`. Reproduced
//! directly against this crate's pinned SQLite: setting WAL first and the
//! pragma second leaves `PRAGMA auto_vacuum` reading `0`, silently, with no
//! error anywhere. See [`crate::state::open`]'s pragma order, which depends on
//! it.

use rusqlite::Connection;

/// A database's `auto_vacuum` mode — the property that decides whether free
/// pages can ever be returned without rewriting the whole file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoVacuum {
    /// Free pages stay in the file forever; only a full `VACUUM` reclaims.
    None,
    /// Every commit trims the file. Not used here: the cost lands on the write
    /// path rather than on idle time.
    Full,
    /// Free pages are reclaimable in bounded chunks by
    /// [`incremental_vacuum`], on the caller's schedule.
    Incremental,
}

impl AutoVacuum {
    /// Parse `PRAGMA auto_vacuum`'s integer; anything unknown reads as
    /// [`AutoVacuum::None`], which is the answer that makes a caller do the
    /// safe thing (offer a full pass) rather than assume a capability.
    pub fn from_db(value: i64) -> Self {
        match value {
            1 => AutoVacuum::Full,
            2 => AutoVacuum::Incremental,
            _ => AutoVacuum::None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AutoVacuum::None => "none",
            AutoVacuum::Full => "full",
            AutoVacuum::Incremental => "incremental",
        }
    }
}

/// What a database occupies and how much of that is dead space.
///
/// Read from three O(1) pragmas. Deliberately **not** from `dbstat`: that
/// virtual table walks every page in the file, which on the store that
/// motivated this module is minutes of I/O — a health check nobody would run
/// twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbSpace {
    pub page_size: u64,
    pub page_count: u64,
    pub freelist_count: u64,
    pub auto_vacuum: AutoVacuum,
}

impl DbSpace {
    /// Bytes the file occupies (excluding `-wal`/`-shm`, which
    /// [`crate::checkpoint::wal_bytes`] answers for).
    pub fn file_bytes(self) -> u64 {
        self.page_size.saturating_mul(self.page_count)
    }

    /// Bytes held by the file but belonging to no row.
    pub fn free_bytes(self) -> u64 {
        self.page_size.saturating_mul(self.freelist_count)
    }

    /// Dead space as a fraction of the file, `0.0` for an empty database.
    pub fn free_ratio(self) -> f64 {
        if self.page_count == 0 {
            return 0.0;
        }
        self.freelist_count as f64 / self.page_count as f64
    }
}

/// Read [`DbSpace`] from an open connection.
pub fn db_space(conn: &Connection) -> rusqlite::Result<DbSpace> {
    let page_size: i64 = conn.query_row("PRAGMA page_size", [], |r| r.get(0))?;
    let page_count: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    let freelist_count: i64 = conn.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    let auto_vacuum: i64 = conn.query_row("PRAGMA auto_vacuum", [], |r| r.get(0))?;
    Ok(DbSpace {
        page_size: page_size.max(0) as u64,
        page_count: page_count.max(0) as u64,
        freelist_count: freelist_count.max(0) as u64,
        auto_vacuum: AutoVacuum::from_db(auto_vacuum),
    })
}

/// Below this the file is left alone whatever its ratio (`X-012`).
///
/// A freshly created store is mostly empty by construction, and a health report
/// that flags a 40 MB file for being half free would train its reader to ignore
/// it. The floor is what makes the signal mean "this is worth an operator's
/// minutes".
pub const RECLAIM_MIN_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// The share of dead space at which reclaiming becomes worth reporting
/// (`X-012`) — chosen, not derived, the same footing as
/// [`crate::checkpoint::WAL_TRUNCATE_THRESHOLD_BYTES`].
pub const RECLAIM_FREE_RATIO: f64 = 0.25;

/// How many pages one idle-time chunk moves (`X-012`).
///
/// At the 4096-byte page this store uses, roughly 8 MiB per call — small enough
/// that a chunk never becomes the freeze this design exists to avoid, large
/// enough that an idle daemon makes visible progress between polls.
pub const INCREMENTAL_VACUUM_PAGES: u32 = 2048;

/// Whether this database is bloated enough to be worth reclaiming.
///
/// Both halves are load-bearing, and `min_file_bytes` is the one that keeps the
/// signal honest — see [`RECLAIM_MIN_FILE_BYTES`]. Thresholds are parameters
/// rather than reads of the constants above so a test can assert *what this
/// reports* without also asserting how big a fixture happens to be: the same
/// reason `D-090` and `D-093` had to make their budgets injectable after
/// hard-coded ones turned assertions into timing.
pub fn should_reclaim(space: &DbSpace, min_file_bytes: u64, free_ratio: f64) -> bool {
    space.file_bytes() >= min_file_bytes && space.free_ratio() >= free_ratio
}

/// Return at most `pages` free pages to the filesystem, answering how many
/// actually went back.
///
/// A no-op returning `0` on a database whose `auto_vacuum` is not
/// `INCREMENTAL`, and deliberately not an error: the caller is a background
/// worker on a store that may predate the conversion, and "there is nothing I
/// can do here" is not a fault worth waking anyone for. Verified against this
/// crate's pinned SQLite — the pragma succeeds with exit status 0 and moves
/// nothing.
pub fn incremental_vacuum(conn: &Connection, pages: u32) -> rusqlite::Result<u64> {
    let before: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    conn.execute_batch(&format!("PRAGMA incremental_vacuum({pages})"))?;
    let after: i64 = conn.query_row("PRAGMA page_count", [], |r| r.get(0))?;
    Ok((before - after).max(0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space(page_count: u64, freelist_count: u64) -> DbSpace {
        DbSpace {
            page_size: 4096,
            page_count,
            freelist_count,
            auto_vacuum: AutoVacuum::Incremental,
        }
    }

    /// The predicate needs both halves, and the floor is the half that keeps a
    /// health report worth reading.
    #[test]
    fn reclaiming_needs_both_a_big_file_and_a_big_share_of_holes() {
        // The store this task came from: 57 GB, 66 % holes.
        let bloated = space(14_875_928, 9_880_851);
        assert!(should_reclaim(
            &bloated,
            RECLAIM_MIN_FILE_BYTES,
            RECLAIM_FREE_RATIO
        ));

        // Same ratio, tiny file — a fresh store, and no operator's problem.
        let small = space(1_000, 900);
        assert!(!should_reclaim(
            &small,
            RECLAIM_MIN_FILE_BYTES,
            RECLAIM_FREE_RATIO
        ));

        // Big file, nothing dead in it.
        let dense = space(14_875_928, 12);
        assert!(!should_reclaim(
            &dense,
            RECLAIM_MIN_FILE_BYTES,
            RECLAIM_FREE_RATIO
        ));
    }

    /// An empty database has no ratio to speak of, and must not divide by zero
    /// to find that out.
    #[test]
    fn an_empty_database_is_never_bloated() {
        let empty = space(0, 0);
        assert_eq!(empty.free_ratio(), 0.0, "no division by zero, and no NaN");
        assert!(!should_reclaim(
            &empty,
            RECLAIM_MIN_FILE_BYTES,
            RECLAIM_FREE_RATIO
        ));
    }

    #[test]
    fn auto_vacuum_parses_and_treats_anything_unknown_as_none() {
        assert_eq!(AutoVacuum::from_db(0), AutoVacuum::None);
        assert_eq!(AutoVacuum::from_db(1), AutoVacuum::Full);
        assert_eq!(AutoVacuum::from_db(2), AutoVacuum::Incremental);
        assert_eq!(AutoVacuum::from_db(7), AutoVacuum::None);
    }

    /// `X-012`: a chunk moves pages on a converted database and answers `0`
    /// without erroring on one that was never converted.
    #[test]
    fn a_chunk_reclaims_on_an_incremental_database_and_is_a_no_op_otherwise() {
        let converted = Connection::open_in_memory().expect("in-memory db");
        converted
            .execute_batch(
                "PRAGMA auto_vacuum = INCREMENTAL;\
                 CREATE TABLE t (x BLOB);",
            )
            .expect("seed");
        for _ in 0..400 {
            converted
                .execute("INSERT INTO t (x) VALUES (randomblob(2000))", [])
                .expect("insert");
        }
        converted.execute("DELETE FROM t", []).expect("delete");
        let freed = incremental_vacuum(&converted, 64).expect("chunk runs");
        assert!(freed > 0, "a converted database must give pages back");

        let plain = Connection::open_in_memory().expect("in-memory db");
        plain
            .execute_batch("CREATE TABLE t (x BLOB);")
            .expect("seed");
        assert_eq!(
            incremental_vacuum(&plain, 64).expect("no-op must not error"),
            0,
            "a store predating the conversion is not a fault to report"
        );
    }
}
