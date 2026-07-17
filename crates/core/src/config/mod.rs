//! The versioned global configuration `<config_dir>/config.toml` (spec 02 §3.1)
//! and the `data_policy` restrictiveness model (spec 02 §3.2, 12 §1).
//!
//! This module owns two things T02-05 introduces:
//!
//! - [`DataPolicy`] — the four-level remote-data policy enum, its canonical
//!   string form, and its **most-restrictive** merge (spec 02 §3.2, 12 §1
//!   `[FIXED]`). The ordering, strictest first, is
//!   `local_only > metadata_only_remote > allow_remote_with_redaction >
//!   allow_remote_full`.
//! - [`Config`] — the typed, validated global config parsed from TOML, with
//!   defaults matching spec 02 §3.1 verbatim.
//!
//! **Config resolution has exactly one input: the resolved `<config_dir>`.**
//! [`Config::load`] reads only `<config_dir>/config.toml`; there is deliberately
//! no API that takes a worktree or repository root, so a repository checkout can
//! never introduce a config file that changes daemon behavior (spec 02 §3.2:
//! "never via files inside the repository").
//!
//! **Validation policy (as-built `[SPEC]` for the gaps 02 §3.1 leaves open):**
//! a missing file yields full [`Config::default`]; an unknown/unsupported
//! `schema_version` is a typed [`ConfigError::UnsupportedSchemaVersion`]; an
//! invalid `data_policy` enum is a typed [`ConfigError::InvalidDataPolicy`] (it
//! is never silently downgraded to the default — spec 02 §6 "nothing degrades
//! silently" `[FIXED]`); unknown TOML keys are ignored (lenient / forward
//! compatible). The `[OPEN]` numbers in spec 02 §3.1
//! (`storage.retired_generations_keep`/`_ttl_h`, `index.languages`) are parsed as
//! provisional defaults matching the spec text — this module does not close those
//! open questions.
//!
//! Per-repository overrides (spec 02 §3.2, the `repo_settings` table) and the
//! effective-policy merge across repos live in `local-rag-store`
//! (`registry::settings`), which reuses [`DataPolicy::most_restrictive`] from
//! here. The central remote-policy guard in the provider pool (spec 10 §1, 12 §1)
//! is a later group (T11/T16); this module only supplies the values it consumes.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The only `schema_version` this binary supports (spec 02 §3.1).
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

/// The data policy governing whether — and how — a request may leave the machine
/// (spec 02 §3.2, 12 §1 `[FIXED]`).
///
/// Variants are ordered from **most** to **least** restrictive. The canonical
/// string forms ([`as_str`](DataPolicy::as_str)) are the snake-case names used in
/// `config.toml`, the `repo_settings` table, and the wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataPolicy {
    /// Never leave the machine; remote providers are refused, never a fallback.
    LocalOnly,
    /// Only non-content metadata may be sent remotely.
    MetadataOnlyRemote,
    /// Remote calls are allowed after redaction.
    AllowRemoteWithRedaction,
    /// Remote calls are allowed with full payloads.
    AllowRemoteFull,
}

impl DataPolicy {
    /// The canonical snake-case string (spec 02 §3.1/§3.2).
    pub fn as_str(self) -> &'static str {
        match self {
            DataPolicy::LocalOnly => "local_only",
            DataPolicy::MetadataOnlyRemote => "metadata_only_remote",
            DataPolicy::AllowRemoteWithRedaction => "allow_remote_with_redaction",
            DataPolicy::AllowRemoteFull => "allow_remote_full",
        }
    }

    /// Parse a canonical string back into a policy, or `None` if unrecognized.
    ///
    /// The inverse of [`as_str`](DataPolicy::as_str); a value outside the four
    /// canonical names yields `None` so callers can raise a typed error rather
    /// than silently defaulting.
    pub fn from_str_value(value: &str) -> Option<DataPolicy> {
        match value {
            "local_only" => Some(DataPolicy::LocalOnly),
            "metadata_only_remote" => Some(DataPolicy::MetadataOnlyRemote),
            "allow_remote_with_redaction" => Some(DataPolicy::AllowRemoteWithRedaction),
            "allow_remote_full" => Some(DataPolicy::AllowRemoteFull),
            _ => None,
        }
    }

    /// Restrictiveness rank: `0` is strictest ([`LocalOnly`](DataPolicy::LocalOnly)),
    /// `3` is loosest ([`AllowRemoteFull`](DataPolicy::AllowRemoteFull)).
    ///
    /// The rank encodes the spec 02 §3.2 order
    /// `local_only > metadata_only_remote > allow_remote_with_redaction >
    /// allow_remote_full` (a smaller rank is more restrictive).
    pub fn restrictiveness_rank(self) -> u8 {
        match self {
            DataPolicy::LocalOnly => 0,
            DataPolicy::MetadataOnlyRemote => 1,
            DataPolicy::AllowRemoteWithRedaction => 2,
            DataPolicy::AllowRemoteFull => 3,
        }
    }

    /// The more restrictive of `self` and `other` (spec 02 §3.2).
    ///
    /// Commutative and associative, so folding it over any set of policies is
    /// order-independent — the effective policy is deterministic regardless of the
    /// order repositories are visited. A tie (equal policies) returns that policy.
    pub fn most_restrictive(self, other: DataPolicy) -> DataPolicy {
        if other.restrictiveness_rank() < self.restrictiveness_rank() {
            other
        } else {
            self
        }
    }
}

impl Default for DataPolicy {
    /// `local_only` (spec 02 §3.1 `[FIXED default]`, 12 §1).
    fn default() -> Self {
        DataPolicy::LocalOnly
    }
}

/// `[daemon]` section of `config.toml` (spec 02 §3.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Idle-shutdown grace period, seconds (only when fully idle).
    pub idle_shutdown_secs: u64,
    /// Shard-manager LRU size.
    pub max_open_shards: u32,
    /// Log level.
    pub log_level: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            idle_shutdown_secs: 900,
            max_open_shards: 8,
            log_level: "info".to_string(),
        }
    }
}

/// `[storage]` section of `config.toml` (spec 02 §3.1).
///
/// `retired_generations_keep`/`retired_generations_ttl_h` are `[OPEN]` in the
/// spec; the values here are the spec's provisional defaults, not a closed answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// LRU eviction target for `cache.sqlite` vectors, MiB.
    pub embedding_cache_budget_mb: u64,
    /// `observation_payload` TTL, hours.
    pub payload_ttl_hours: u64,
    /// Retained retired generations, `K` (spec `[OPEN]`).
    pub retired_generations_keep: u32,
    /// Retired-generation TTL, `T` hours (spec `[OPEN]`).
    pub retired_generations_ttl_h: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        StorageConfig {
            embedding_cache_budget_mb: 2048,
            payload_ttl_hours: 72,
            retired_generations_keep: 2,
            retired_generations_ttl_h: 168,
        }
    }
}

/// `[models]` section of `config.toml`, validated (spec 02 §3.1).
///
/// The keys mirror the per-repository `[models]` keys in `repo_settings`
/// (spec 02 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelsConfig {
    /// Model-space name resolved against the `state.sqlite` registry.
    pub default_model_space: String,
    /// Global data policy (spec 02 §3.2, 12 §1).
    pub data_policy: DataPolicy,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        ModelsConfig {
            default_model_space: "default".to_string(),
            data_policy: DataPolicy::default(),
        }
    }
}

/// `[index]` section of `config.toml` (spec 02 §3.1).
///
/// `languages` is the `[OPEN]` first-release language set; the value here is the
/// spec's provisional default, not a closed answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct IndexConfig {
    /// Languages to index (spec `[OPEN]`).
    pub languages: Vec<String>,
    /// Maximum indexed file size, KiB.
    pub max_file_size_kb: u64,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig {
            languages: vec!["typescript".to_string(), "javascript".to_string()],
            max_file_size_kb: 1024,
        }
    }
}

/// The typed, validated global configuration (spec 02 §3.1).
///
/// Build it with [`Config::load`] (from a resolved `<config_dir>`),
/// [`Config::parse_toml`] (from TOML text), or [`Config::default`] (the spec
/// defaults). Every field is populated: missing TOML keys fall back to the
/// per-section defaults above.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Config schema version; always [`SUPPORTED_SCHEMA_VERSION`] once validated.
    pub schema_version: u32,
    /// `[daemon]` section.
    pub daemon: DaemonConfig,
    /// `[storage]` section.
    pub storage: StorageConfig,
    /// `[models]` section.
    pub models: ModelsConfig,
    /// `[index]` section.
    pub index: IndexConfig,
}

impl Default for Config {
    /// The spec 02 §3.1 defaults verbatim.
    fn default() -> Self {
        Config {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            daemon: DaemonConfig::default(),
            storage: StorageConfig::default(),
            models: ModelsConfig::default(),
            index: IndexConfig::default(),
        }
    }
}

impl Config {
    /// Parse and validate `config.toml` text.
    ///
    /// Unknown keys are ignored; missing keys default per section. Validation is
    /// explicit: an unsupported `schema_version` or an invalid `data_policy`
    /// value is a typed [`ConfigError`].
    pub fn parse_toml(text: &str) -> Result<Config, ConfigError> {
        let raw: RawConfig = toml::from_str(text).map_err(ConfigError::Toml)?;
        Config::from_raw(raw)
    }

    /// Load the global config from `<config_dir>/config.toml`.
    ///
    /// A missing file (`NotFound`) yields [`Config::default`]; any other I/O error
    /// is a typed [`ConfigError::Io`]. The single input is `config_dir` — this
    /// function never consults a worktree or repository tree (spec 02 §3.2).
    pub fn load(config_dir: &Path) -> Result<Config, ConfigError> {
        let path = config_toml_path(config_dir);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse_toml(&text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(err) => Err(ConfigError::Io(err)),
        }
    }

    /// Validate a deserialized [`RawConfig`] into a typed [`Config`].
    fn from_raw(raw: RawConfig) -> Result<Config, ConfigError> {
        if raw.schema_version != SUPPORTED_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchemaVersion {
                found: raw.schema_version,
                supported: SUPPORTED_SCHEMA_VERSION,
            });
        }
        let data_policy = DataPolicy::from_str_value(&raw.models.data_policy).ok_or(
            ConfigError::InvalidDataPolicy {
                value: raw.models.data_policy.clone(),
            },
        )?;
        Ok(Config {
            schema_version: raw.schema_version,
            daemon: raw.daemon,
            storage: raw.storage,
            models: ModelsConfig {
                default_model_space: raw.models.default_model_space,
                data_policy,
            },
            index: raw.index,
        })
    }
}

/// The `<config_dir>/config.toml` path (spec 02 §3.1).
pub fn config_toml_path(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

/// A failure loading or validating the global config.
#[derive(Debug)]
pub enum ConfigError {
    /// Reading `config.toml` failed (other than the file being absent).
    Io(std::io::Error),
    /// The file is not valid TOML.
    Toml(toml::de::Error),
    /// `schema_version` is not the supported version.
    UnsupportedSchemaVersion {
        /// The version found in the file.
        found: u32,
        /// The version this binary supports ([`SUPPORTED_SCHEMA_VERSION`]).
        supported: u32,
    },
    /// `models.data_policy` is not one of the four canonical values.
    InvalidDataPolicy {
        /// The rejected value.
        value: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(err) => write!(f, "reading config.toml failed: {err}"),
            ConfigError::Toml(err) => write!(f, "config.toml is not valid TOML: {err}"),
            ConfigError::UnsupportedSchemaVersion { found, supported } => write!(
                f,
                "config.toml schema_version {found} is unsupported (this binary supports {supported})"
            ),
            ConfigError::InvalidDataPolicy { value } => write!(
                f,
                "config.toml models.data_policy {value:?} is not one of \
                 local_only | metadata_only_remote | allow_remote_with_redaction | allow_remote_full"
            ),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            ConfigError::Toml(err) => Some(err),
            ConfigError::UnsupportedSchemaVersion { .. }
            | ConfigError::InvalidDataPolicy { .. } => None,
        }
    }
}

/// The lenient deserialization target: primitive fields, unknown keys ignored,
/// missing keys defaulted per section. Validated into [`Config`] by
/// [`Config::from_raw`].
#[derive(Deserialize)]
#[serde(default)]
struct RawConfig {
    schema_version: u32,
    daemon: DaemonConfig,
    storage: StorageConfig,
    models: RawModels,
    index: IndexConfig,
}

impl Default for RawConfig {
    fn default() -> Self {
        RawConfig {
            schema_version: SUPPORTED_SCHEMA_VERSION,
            daemon: DaemonConfig::default(),
            storage: StorageConfig::default(),
            models: RawModels::default(),
            index: IndexConfig::default(),
        }
    }
}

/// `[models]` before validation: `data_policy` is still an unparsed string.
#[derive(Deserialize)]
#[serde(default)]
struct RawModels {
    default_model_space: String,
    data_policy: String,
}

impl Default for RawModels {
    fn default() -> Self {
        RawModels {
            default_model_space: "default".to_string(),
            data_policy: DataPolicy::default().as_str().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verbatim spec 02 §3.1 config, used to keep [`Config::default`] in sync
    /// with the normative text (any drift fails `default_matches_spec_toml`).
    const SPEC_CONFIG_TOML: &str = "\
schema_version = 1

[daemon]
idle_shutdown_secs   = 900
max_open_shards      = 8
log_level            = \"info\"

[storage]
embedding_cache_budget_mb = 2048
payload_ttl_hours         = 72
retired_generations_keep  = 2
retired_generations_ttl_h = 168

[models]
default_model_space = \"default\"
data_policy = \"local_only\"

[index]
languages = [\"typescript\", \"javascript\"]
max_file_size_kb = 1024
";

    const ALL_POLICIES: [DataPolicy; 4] = [
        DataPolicy::LocalOnly,
        DataPolicy::MetadataOnlyRemote,
        DataPolicy::AllowRemoteWithRedaction,
        DataPolicy::AllowRemoteFull,
    ];

    #[test]
    fn data_policy_string_round_trips_and_rejects_bogus() {
        for p in ALL_POLICIES {
            assert_eq!(DataPolicy::from_str_value(p.as_str()), Some(p));
        }
        assert_eq!(DataPolicy::from_str_value("bogus"), None);
        assert_eq!(DataPolicy::from_str_value("LOCAL_ONLY"), None);
        assert_eq!(DataPolicy::from_str_value(""), None);
    }

    #[test]
    fn data_policy_default_is_local_only() {
        assert_eq!(DataPolicy::default(), DataPolicy::LocalOnly);
    }

    #[test]
    fn most_restrictive_covers_every_pair() {
        // The strictest of the pair (smallest rank) wins, for all 16 pairs; the
        // operation is commutative and idempotent on the diagonal.
        for a in ALL_POLICIES {
            assert_eq!(a.most_restrictive(a), a, "idempotent on {a:?}");
            for b in ALL_POLICIES {
                let expected = if a.restrictiveness_rank() <= b.restrictiveness_rank() {
                    a
                } else {
                    b
                };
                assert_eq!(a.most_restrictive(b), expected, "{a:?} vs {b:?}");
                assert_eq!(
                    a.most_restrictive(b),
                    b.most_restrictive(a),
                    "commutative {a:?}/{b:?}"
                );
            }
        }
        // Spot-check the spec order explicitly.
        assert_eq!(
            DataPolicy::LocalOnly.most_restrictive(DataPolicy::AllowRemoteFull),
            DataPolicy::LocalOnly,
        );
        assert_eq!(
            DataPolicy::MetadataOnlyRemote.most_restrictive(DataPolicy::AllowRemoteWithRedaction),
            DataPolicy::MetadataOnlyRemote,
        );
    }

    #[test]
    fn default_matches_spec_toml() {
        assert_eq!(
            Config::parse_toml(SPEC_CONFIG_TOML).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn empty_toml_is_all_defaults() {
        assert_eq!(Config::parse_toml("").unwrap(), Config::default());
    }

    #[test]
    fn partial_toml_defaults_missing_keys_and_sections() {
        let cfg = Config::parse_toml("[models]\ndata_policy = \"allow_remote_full\"\n").unwrap();
        assert_eq!(cfg.models.data_policy, DataPolicy::AllowRemoteFull);
        // Missing key in a present section defaults.
        assert_eq!(cfg.models.default_model_space, "default");
        // Missing sections default wholesale.
        assert_eq!(cfg.daemon, DaemonConfig::default());
        assert_eq!(cfg.index, IndexConfig::default());
        assert_eq!(cfg.schema_version, SUPPORTED_SCHEMA_VERSION);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let cfg = Config::parse_toml(
            "schema_version = 1\nmystery = 7\n\n[models]\ndata_policy = \"local_only\"\nfuture_key = \"x\"\n\n[unknown_section]\nk = 1\n",
        )
        .expect("unknown keys are tolerated");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn unsupported_schema_version_is_rejected() {
        let err = Config::parse_toml("schema_version = 2\n").unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::UnsupportedSchemaVersion {
                    found: 2,
                    supported: 1
                }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn invalid_data_policy_is_rejected_not_defaulted() {
        let err = Config::parse_toml("[models]\ndata_policy = \"yolo_remote\"\n").unwrap_err();
        match err {
            ConfigError::InvalidDataPolicy { value } => assert_eq!(value, "yolo_remote"),
            other => panic!("expected InvalidDataPolicy, got {other:?}"),
        }
    }

    #[test]
    fn malformed_toml_is_rejected() {
        assert!(matches!(
            Config::parse_toml("this is = = not toml").unwrap_err(),
            ConfigError::Toml(_)
        ));
    }

    #[test]
    fn config_toml_path_joins_filename() {
        assert_eq!(
            config_toml_path(Path::new("/x/config")),
            PathBuf::from("/x/config/config.toml")
        );
    }
}
