//! Identity primitives: the building blocks of every durable ID (spec 01 §5,
//! 03 §1).
//!
//! Two rules from spec 01 §5 shape this module:
//!
//! - **No durable ID is derived from a filesystem path.** A path-derived hash is
//!   allowed only as a *lookup key* ([`domain::path_fingerprint`]), never as an
//!   FK target. Durable identities are either random ([`uuidv7`]) or
//!   content/manifest hashes over path-independent inputs ([`domain`]).
//! - **Identity never depends on the display form.** [`path`] canonicalization
//!   returns a [`path::Canonical`] that carries both the identity form and the
//!   preserved display spelling; only the former ever feeds a hash.
//!
//! Every primitive is split into a pure core (deterministic, no clock/entropy)
//! and a thin OS-backed wrapper, mirroring the `Env`/`Clock`/`IdSource` seams
//! already used across the workspace, so behavior is unit-testable without
//! touching the wall clock, the environment, or the filesystem.

pub mod domain;
pub mod path;
pub mod remote;
pub mod uuidv7;

pub use domain::{Domain, HASH_SCHEMA_VERSION};
pub use path::{Canonical, CaseSensitivity};
pub use uuidv7::{Uuid, UuidSource, uuidv7_from};

#[cfg(unix)]
pub use uuidv7::SystemUuidV7;
