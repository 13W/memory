//! The daemon's serving mode (spec 02 §6: "Migration required/running →
//! daemon serves only health/status → `MIGRATION_IN_PROGRESS`"; 13 §3: "the
//! daemon otherwise quiescent") — T15-01.
//!
//! `lifecycle::run` (spec 02 §4.1 step 2) sets [`DaemonMode::MigrationOnly`]
//! when the migration framework refuses to touch the store — a newer schema
//! than this binary supports, a checksum drift, or rewritten migration
//! history (`local_rag_store::migrate::MigrationError`'s own typed
//! variants; spec 02 §6's own text names exactly these three as
//! `INCOMPATIBLE_STORE`, disambiguated by `details`). Steps 3/5 are skipped
//! in that case, but step 4 still runs — the socket still binds and the
//! lock still gets marked ready — so the condition is diagnosable (spec 02
//! §6 `[FIXED]`: "nothing degrades silently") rather than a bare
//! crash-exit with no reachable diagnostic surface at all.

/// Whether the daemon is serving normally or only health/status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonMode {
    /// Serving normally.
    Normal,
    /// Only health/status is served (spec 02 §6). `reason` names why.
    MigrationOnly {
        /// Why migration could not proceed.
        reason: MigrationOnlyReason,
    },
}

impl DaemonMode {
    /// The wire-format string this crate's [`super::probe::Greeting::mode`]
    /// and, later, the health/status MCP tool (T15-04) both use.
    pub fn as_str(&self) -> &'static str {
        match self {
            DaemonMode::Normal => "normal",
            DaemonMode::MigrationOnly { .. } => "migration_only",
        }
    }
}

/// Why the daemon could not migrate the store and entered
/// [`DaemonMode::MigrationOnly`] — spec 02 §6's own disambiguation text for
/// `INCOMPATIBLE_STORE`: "a store newer than the binary, a migration
/// checksum drift, or rewritten migration history".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOnlyReason {
    /// The store's schema is newer than this binary supports (spec 13 §3).
    IncompatibleStore {
        /// The maximum version recorded in the store.
        store_version: u32,
        /// The maximum version this binary knows how to apply.
        binary_max_version: u32,
    },
    /// An already-applied migration's recorded checksum diverges from this
    /// binary's SQL for that version (spec 13 §3).
    ChecksumDrift {
        /// The version whose checksum diverged.
        version: u32,
        /// The name recorded in the store for that version.
        name: String,
    },
    /// Some other migration-framework refusal (e.g. rewritten history, a
    /// malformed migration set, a backup/lock/I/O failure) — carries a
    /// description rather than a growing set of near-duplicate variants;
    /// `local_rag_store::migrate::MigrationError` remains the authoritative
    /// typed source, this is a display-only echo of it.
    Other {
        /// A human-readable description of the underlying
        /// `MigrationError`.
        detail: String,
    },
}
