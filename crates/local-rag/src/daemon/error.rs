//! Mapping `local_rag_store::migrate::MigrationError` into
//! [`MigrationOnlyReason`] and the canonical daemon-protocol error envelope
//! (spec 02 §6) — T15-01.
//!
//! `local_rag_protocol::error`'s own module doc is explicit that
//! `IncompatibleStore`'s `details` is filled in by the caller from this
//! already-typed source, never re-derived (`local-rag-protocol` gains no
//! `store` dependency to do that itself); this module is that caller.

use local_rag_protocol::ErrorEnvelope;
use local_rag_store::migrate::MigrationError;

use super::mode::MigrationOnlyReason;

/// Classify a startup migration failure (spec 02 §4.1 step 2) into
/// [`MigrationOnlyReason`].
pub fn migration_only_reason(e: &MigrationError) -> MigrationOnlyReason {
    match e {
        MigrationError::IncompatibleStore {
            store_version,
            binary_max_version,
        } => MigrationOnlyReason::IncompatibleStore {
            store_version: *store_version,
            binary_max_version: *binary_max_version,
        },
        MigrationError::ChecksumDrift { version, name, .. } => MigrationOnlyReason::ChecksumDrift {
            version: *version,
            name: name.clone(),
        },
        other => MigrationOnlyReason::Other {
            detail: other.to_string(),
        },
    }
}

/// The canonical `INCOMPATIBLE_STORE` envelope for a [`MigrationOnlyReason`]
/// (spec 02 §6's own examples: `"store_version 3 > binary_max 2"` /
/// `"checksum drift at version 1"`).
pub fn error_envelope(reason: &MigrationOnlyReason) -> ErrorEnvelope {
    match reason {
        MigrationOnlyReason::IncompatibleStore {
            store_version,
            binary_max_version,
        } => ErrorEnvelope::incompatible_store(format!(
            "store_version {store_version} > binary_max {binary_max_version}"
        )),
        MigrationOnlyReason::ChecksumDrift { version, name } => ErrorEnvelope::incompatible_store(
            format!("checksum drift at version {version} ({name})"),
        ),
        MigrationOnlyReason::Other { detail } => ErrorEnvelope::incompatible_store(detail.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use local_rag_protocol::ErrorCode;

    #[test]
    fn incompatible_store_names_both_versions() {
        let reason = migration_only_reason(&MigrationError::IncompatibleStore {
            store_version: 3,
            binary_max_version: 2,
        });
        assert_eq!(
            reason,
            MigrationOnlyReason::IncompatibleStore {
                store_version: 3,
                binary_max_version: 2,
            }
        );
        let env = error_envelope(&reason);
        assert_eq!(env.code, ErrorCode::IncompatibleStore);
        assert_eq!(
            env.details.as_deref(),
            Some("store_version 3 > binary_max 2")
        );
    }

    #[test]
    fn checksum_drift_names_the_version_and_migration_name() {
        let reason = migration_only_reason(&MigrationError::ChecksumDrift {
            version: 1,
            name: "repository_registry".to_string(),
            expected: "aaa".to_string(),
            found: "bbb".to_string(),
        });
        assert_eq!(
            reason,
            MigrationOnlyReason::ChecksumDrift {
                version: 1,
                name: "repository_registry".to_string(),
            }
        );
        let env = error_envelope(&reason);
        assert_eq!(env.code, ErrorCode::IncompatibleStore);
        let details = env.details.expect("details");
        assert!(details.contains('1'), "{details}");
        assert!(details.contains("repository_registry"), "{details}");
    }

    #[test]
    fn other_migration_errors_fall_back_to_their_display_text() {
        let reason = migration_only_reason(&MigrationError::UnknownAppliedVersion { version: 9 });
        match &reason {
            MigrationOnlyReason::Other { detail } => assert!(detail.contains('9'), "{detail}"),
            other => panic!("expected Other, got {other:?}"),
        }
        let env = error_envelope(&reason);
        assert_eq!(env.code, ErrorCode::IncompatibleStore);
    }
}
