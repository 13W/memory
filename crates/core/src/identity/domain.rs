//! Domain-separated, version-tagged BLAKE3 hashing (spec 03 §1.2).
//!
//! Every content/manifest/subject hash in `local-rag` is computed over a
//! canonical, length-prefixed encoding so that field boundaries are
//! unambiguous and each logical hash lives in its own namespace:
//!
//! ```text
//! H(domain, f₀, f₁, …) = blake3( utf8(domain) ‖ 0x00 ‖ concat( le_u32(len(fᵢ)) ‖ fᵢ ) )
//! ```
//!
//! where `domain` is `local-rag/<HASH_SCHEMA_VERSION>/<slug>` (e.g.
//! `local-rag/1/occurrence_id`). Length-prefixing means `("ab","c")` and
//! `("a","bc")` never collide; changing a domain's field order or count is a
//! breaking change that requires a new [`HASH_SCHEMA_VERSION`] (spec 03 §5).
//!
//! ## Field encoding `[SPEC]`
//!
//! Fields are raw byte strings. Callers serialize higher-level values before
//! hashing; the conventions this crate commits to are:
//!
//! - text fields → UTF-8 bytes;
//! - already-hex identities (UUIDs, other domain hashes) → their lowercase
//!   ASCII bytes, exactly as stored in `TEXT` columns;
//! - fixed-width integer fields (e.g. `algo_version`) → little-endian bytes of
//!   the declared width.
//!
//! Only the two path/remote *lookup* fingerprints owned by the registry
//! ([`path_fingerprint`], [`remote_fingerprint`]) get typed constructors here;
//! the deterministic-ID domains ([`Domain::OccurrenceId`], the projection/FTS
//! manifests, embedding subjects, `memory_op`) are hashed through the generic
//! [`hash`] entry point by their owning tasks, which assemble the fields in the
//! order fixed by the spec table.

/// Hash-schema version embedded in every domain string (spec 03 §1.2 / §5).
///
/// Bumping this invalidates every deterministic ID and is a full-store
/// migration event; it MUST NOT change as an implementation convenience.
pub const HASH_SCHEMA_VERSION: u32 = 1;

/// The defined hash domains (spec 03 §1.2 table). The on-the-wire domain string
/// is `local-rag/<HASH_SCHEMA_VERSION>/<slug>` where `<slug>` is [`Domain::slug`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Domain {
    /// `file_revision.content_hash` — raw file bytes.
    FileContent,
    /// `content_blob.blob_id`.
    ContentBlob,
    /// `generation_unit_occurrence.occurrence_id`.
    OccurrenceId,
    /// Dense projection point ID (05 §3).
    ProjectionPoint,
    /// `ProjectionHead.manifest_hash`.
    ProjectionManifest,
    /// `fts_projection_head.manifest_hash`.
    FtsManifest,
    /// `embedding_cache.subject_hash` for a shared content blob.
    SubjectContentBlob,
    /// `embedding_cache.subject_hash` for a per-occurrence context.
    SubjectOccurrenceContext,
    /// `embedding_cache.subject_hash` for a memory entry.
    SubjectMemoryEntry,
    /// `worktree_path.path_fingerprint` — lookup accelerator only, never an
    /// identity (spec 01 §5, 03 §2.1).
    PathFingerprint,
    /// `repository.git_remote_fingerprint` — a hint, nullable and NOT unique.
    RemoteFingerprint,
    /// Consolidation idempotency key.
    MemoryOp,
}

impl Domain {
    /// The domain's stable slug (the trailing segment of the domain string).
    /// Two subject domains intentionally carry a `/` (`subject/content_blob`).
    pub const fn slug(self) -> &'static str {
        match self {
            Domain::FileContent => "file_content",
            Domain::ContentBlob => "content_blob",
            Domain::OccurrenceId => "occurrence_id",
            Domain::ProjectionPoint => "projection_point",
            Domain::ProjectionManifest => "projection_manifest",
            Domain::FtsManifest => "fts_manifest",
            Domain::SubjectContentBlob => "subject/content_blob",
            Domain::SubjectOccurrenceContext => "subject/occurrence_context",
            Domain::SubjectMemoryEntry => "subject/memory_entry",
            Domain::PathFingerprint => "path_fingerprint",
            Domain::RemoteFingerprint => "remote_fingerprint",
            Domain::MemoryOp => "memory_op",
        }
    }

    /// The fully-qualified, version-tagged domain string
    /// (`local-rag/<HASH_SCHEMA_VERSION>/<slug>`).
    pub fn qualified(self) -> String {
        format!("local-rag/{HASH_SCHEMA_VERSION}/{}", self.slug())
    }
}

/// Encode the canonical hash input for `domain` and `fields` (spec 03 §1.2):
/// `utf8(domain) ‖ 0x00 ‖ concat( le_u32(len(fᵢ)) ‖ fᵢ )`.
///
/// Exposed for byte-exact testing and for callers that need the pre-image; most
/// callers want [`hash`]. Each field must be at most `u32::MAX` bytes (all real
/// inputs — paths, IDs, size-capped file content — are far smaller).
///
/// # Panics
///
/// Panics if any field exceeds `u32::MAX` bytes, which would make the
/// length prefix ambiguous.
pub fn encode(domain: Domain, fields: &[&[u8]]) -> Vec<u8> {
    let qualified = domain.qualified();
    let capacity = qualified.len() + 1 + fields.iter().map(|f| 4 + f.len()).sum::<usize>();
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(qualified.as_bytes());
    out.push(0x00);
    for field in fields {
        let len = u32::try_from(field.len()).expect("hash field exceeds u32::MAX bytes");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Domain-separated BLAKE3 digest of `fields`, as 64 lowercase hex characters.
pub fn hash(domain: Domain, fields: &[&[u8]]) -> String {
    blake3::hash(&encode(domain, fields)).to_hex().to_string()
}

/// `H(path_fingerprint, canonical_path)` — the `worktree_path.path_fingerprint`
/// lookup key (spec 03 §1.2 / §2.1). This is deliberately a *lookup* hash, not a
/// durable identity: worktree identity is a random UUID, never path-derived
/// (spec 01 §5).
pub fn path_fingerprint(canonical_path: &str) -> String {
    hash(Domain::PathFingerprint, &[canonical_path.as_bytes()])
}

/// `H(remote_fingerprint, normalized_remote_url)` — the
/// `repository.git_remote_fingerprint` hint (spec 03 §1.2 / §2.1). The input
/// must already be normalized (see [`crate::identity::remote`]); the resulting
/// fingerprint is a hint, nullable and NOT unique.
pub fn remote_fingerprint(normalized_remote_url: &str) -> String {
    hash(
        Domain::RemoteFingerprint,
        &[normalized_remote_url.as_bytes()],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Raw BLAKE3 known-answer tests -------------------------------------
    //
    // Ground truth is the published BLAKE3 reference `test_vectors.json`: inputs
    // are the cyclic byte pattern 0,1,…,250,0,1,… of the given length, and the
    // 32-byte digests below are the project's own published values. Matching
    // them proves the linked `blake3` crate is the reference algorithm
    // (including multi-chunk tree hashing at the 1024-byte chunk boundary), not
    // a circular self-check.

    fn cyclic(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn raw_hex(bytes: &[u8]) -> String {
        blake3::hash(bytes).to_hex().to_string()
    }

    #[test]
    fn blake3_reference_vectors() {
        assert_eq!(
            raw_hex(&cyclic(0)),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
        );
        assert_eq!(
            raw_hex(&cyclic(1)),
            "2d3adedff11b61f14c886e35afa036736dcd87a74d27b5c1510225d0f592e213",
        );
        assert_eq!(
            raw_hex(&cyclic(3)),
            "e1be4d7a8ab5560aa4199eea339849ba8e293d55ca0a81006726d184519e647f",
        );
        // Crosses the 1024-byte chunk boundary → exercises the tree hasher.
        assert_eq!(
            raw_hex(&cyclic(1024)),
            "42214739f095a406f3fc83deb889744ac00df831c10daa55189b5d121c855af7",
        );
    }

    // ---- Domain string + framing -------------------------------------------

    #[test]
    fn qualified_domain_strings_are_version_tagged() {
        assert_eq!(
            Domain::OccurrenceId.qualified(),
            "local-rag/1/occurrence_id"
        );
        assert_eq!(
            Domain::SubjectContentBlob.qualified(),
            "local-rag/1/subject/content_blob",
        );
        assert_eq!(HASH_SCHEMA_VERSION, 1);
    }

    #[test]
    fn encode_produces_exact_bytes() {
        // Hand-built pre-image, independent of `encode`, pins the framing:
        // domain ‖ 0x00 ‖ (le_u32(2) ‖ "ab") ‖ (le_u32(1) ‖ "c").
        let mut expected = b"local-rag/1/occurrence_id".to_vec();
        expected.push(0x00);
        expected.extend_from_slice(&2u32.to_le_bytes());
        expected.extend_from_slice(b"ab");
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(b"c");
        assert_eq!(encode(Domain::OccurrenceId, &[b"ab", b"c"]), expected);
    }

    #[test]
    fn empty_field_list_hashes_domain_only() {
        // domain ‖ 0x00, no field bytes.
        assert_eq!(encode(Domain::FileContent, &[]), {
            let mut v = b"local-rag/1/file_content".to_vec();
            v.push(0x00);
            v
        });
        assert_eq!(
            hash(Domain::FileContent, &[]),
            "8b79c0ffcc4c357c73fc2245c71a5657faafb980f7f86e1eb7aa7f395061caaa",
        );
    }

    // ---- Golden digest per domain ------------------------------------------
    //
    // Fixed input [0x01 0x02 0x03, "abc"] hashed under every domain. Same fields,
    // different domain string ⇒ every digest differs (domain separation). The
    // constants are locked against accidental drift in the domain slug/framing.

    const GOLDEN_FIELDS: &[&[u8]] = &[&[0x01u8, 0x02, 0x03], b"abc"];

    #[test]
    fn golden_hashes_for_every_domain() {
        let table = [
            (
                Domain::FileContent,
                "b3bdadb5ece1ccbc2a9054751bf4e92b340e00f47c5067a295b1f579e02a0077",
            ),
            (
                Domain::ContentBlob,
                "6a6b4afdd576f05ca96a0d6dfb0f12cb794027df06770164201f89ad7254a21f",
            ),
            (
                Domain::OccurrenceId,
                "7aed9f4f28b74e0ec9f9354eb66e986a070de9d8bbe12344063865a6e70f4d89",
            ),
            (
                Domain::ProjectionPoint,
                "e679cacd399c83c4d6df583e1a45927e576169e94e611794a80db74dc20e9d33",
            ),
            (
                Domain::ProjectionManifest,
                "c6654468f0ef3b8fb4c7bd7b331f291f3b31ae69c81be1aec69841eee3ad516f",
            ),
            (
                Domain::FtsManifest,
                "383629e90e868e284bba6fa5f1a0b96080bfb858110d745c603796a758c28be9",
            ),
            (
                Domain::SubjectContentBlob,
                "4c5d42a635a7e5ca9886be959a07818474ddd712a2d554b090c51e96d56001e3",
            ),
            (
                Domain::SubjectOccurrenceContext,
                "36d25245108461f6abfa35f63c360f35918c77b5d8f8026233af276d8feb3c86",
            ),
            (
                Domain::SubjectMemoryEntry,
                "9fd0c2814011fe651569cc59dbc2c4cbcd373ea770602f9f72c5dc3f42295673",
            ),
            (
                Domain::PathFingerprint,
                "c31a7f2ecb1f127b9667f6c734da16a1a2707277ed962828f09169553ff07564",
            ),
            (
                Domain::RemoteFingerprint,
                "70d10cab888f113e581ffb636087a634b0d788086facdee063745f2a43ec50a8",
            ),
            (
                Domain::MemoryOp,
                "90cafde9944e4e623bcf4bbf83acacf6f722724268b4cca95dc1b0408253e078",
            ),
        ];
        for (domain, expected) in table {
            assert_eq!(hash(domain, GOLDEN_FIELDS), expected, "domain {domain:?}");
        }
        // Domain separation: all twelve digests are distinct.
        let mut seen: Vec<&str> = table.iter().map(|(_, h)| *h).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), table.len());
    }

    // ---- Field-boundary collision regression -------------------------------

    #[test]
    fn field_boundaries_do_not_collide() {
        // Naive concatenation would make these identical ("abc"); length-
        // prefixing keeps them distinct.
        let a = hash(Domain::OccurrenceId, &[b"ab", b"c"]);
        let b = hash(Domain::OccurrenceId, &[b"a", b"bc"]);
        assert_ne!(a, b);
        assert_eq!(
            a,
            "d7fe72fc2540c5df80023be19ca3281575eb8ff15ca430fbaed6cc9c1dc3ae2f"
        );
        assert_eq!(
            b,
            "d2f13be18e9805ef5c4f2f4bd8b7cd77c70c5fc7f79e09f89bf5ebc453bb3b44"
        );
    }

    // ---- Deterministic under order / retry ---------------------------------

    #[test]
    fn hash_is_deterministic_under_retry() {
        let once = hash(Domain::OccurrenceId, GOLDEN_FIELDS);
        for _ in 0..8 {
            assert_eq!(hash(Domain::OccurrenceId, GOLDEN_FIELDS), once);
        }
    }

    // ---- Typed lookup fingerprints -----------------------------------------

    #[test]
    fn typed_fingerprints_match_generic() {
        assert_eq!(
            path_fingerprint("src/main.rs"),
            hash(Domain::PathFingerprint, &[b"src/main.rs"]),
        );
        assert_eq!(
            path_fingerprint("src/main.rs"),
            "b4f780a2e385b099435a5f551fa248d11a7a606e627a8b3816f95a4e437c46f0",
        );
        assert_eq!(
            remote_fingerprint("github.com/org/repo"),
            "a00bd1a5288c0359548d80f6d56c002a4c3262120ffdbcd8a02b4afa25b8f2c3",
        );
    }

    #[test]
    fn digest_is_lowercase_hex_of_len_64() {
        let digest = hash(Domain::OccurrenceId, GOLDEN_FIELDS);
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
    }
}
