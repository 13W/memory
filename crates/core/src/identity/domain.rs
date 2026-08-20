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
//! The single-field fingerprints owned outside the registry
//! ([`path_fingerprint`], [`remote_fingerprint`], [`signature_fingerprint`]) get
//! typed constructors here; the multi-field deterministic-ID domains
//! ([`Domain::OccurrenceId`], the projection/FTS manifests, embedding subjects,
//! `memory_op`) are hashed through the generic [`hash`] entry point by their
//! owning tasks, which assemble the fields in the order fixed by the spec table.

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
    /// `parsed_unit.syntax_locator` signature fingerprint — the `sig` field of a
    /// `SyntaxLocator` (spec 03 §2.4). Derived from the parse subtree only
    /// (never from a path/offset), so it is path-free and stable under unrelated
    /// edits; hashing makes the serialized locator delimiter-safe by
    /// construction (ADR-0002).
    SignatureFingerprint,
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
    /// The `hash` half of the `{hash, original_size}` metadata a size-capped
    /// excerpt leaves behind (spec 12 §2 `[FIXED]`) — the search snippet's
    /// 8 KiB cap (09 §7, T12-04) today, memory evidence's 4 KiB cap later.
    ///
    /// Its own domain rather than a reuse of [`Domain::FileContent`]: an
    /// excerpt is a *slice* of a file, and a snippet that happened to equal a
    /// whole small file would otherwise hash identically to that file's
    /// `content_hash` — exactly the confusion domain separation exists to
    /// prevent.
    TruncatedExcerpt,
}

impl Domain {
    /// The domain's stable slug (the trailing segment of the domain string).
    /// Two subject domains intentionally carry a `/` (`subject/content_blob`).
    pub const fn slug(self) -> &'static str {
        match self {
            Domain::FileContent => "file_content",
            Domain::ContentBlob => "content_blob",
            Domain::SignatureFingerprint => "signature_fingerprint",
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
            Domain::TruncatedExcerpt => "truncated_excerpt",
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

/// `H(truncated_excerpt, bytes)` — the `hash` half of spec 12 §2's
/// `{hash, original_size}` truncation metadata, taken over the **full**
/// (pre-truncation) excerpt so a caller can tell what was cut without keeping
/// it. Joins the single-field typed-constructor precedent below.
pub fn truncated_excerpt(bytes: &[u8]) -> String {
    hash(Domain::TruncatedExcerpt, &[bytes])
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

/// `H(signature_fingerprint, canonical_descriptor)` — the `sig` field of a
/// `SyntaxLocator` (spec 03 §2.4, ADR-0002).
///
/// The caller (a parser adapter) assembles a single canonical, deterministic
/// descriptor string of the unit's signature from the parse subtree only. It is
/// hashed as **one** field, so the domain's field count stays fixed at 1 and the
/// descriptor's internal structure is free to evolve within a `queries=` /
/// `grammar=` rebuild event without a [`HASH_SCHEMA_VERSION`] bump. Hashing also
/// makes the value delimiter-safe (64 lowercase hex, no `;`/`=`) for the locator
/// serialization.
pub fn signature_fingerprint(canonical_descriptor: &str) -> String {
    hash(
        Domain::SignatureFingerprint,
        &[canonical_descriptor.as_bytes()],
    )
}

/// `H(subject/content_blob, blob_id)` — an `embedding_cache.subject_hash` for the
/// `content_blob` kind (spec 03 §1.2 / §4.2, T11-02). One field: the *already*
/// content-derived [`crate::code`]-style `blob_id`, hashed as its lowercase ASCII
/// bytes (the codebase's "already-hex identity" convention) — this is a second
/// hash layer over `blob_id`, never a re-hash of the underlying normalized text.
/// Two occurrences whose `parsed_unit.blob_id` coincide therefore resolve to the
/// same subject hash and share one `embedding_cache` row (spec 03 §4.2's "content
/// shares across occurrences" `[FIXED]`).
pub fn subject_content_blob(blob_id: &str) -> String {
    hash(Domain::SubjectContentBlob, &[blob_id.as_bytes()])
}

/// `H(subject/occurrence_context, context_version, serialization)` — an
/// `embedding_cache.subject_hash` for the `occurrence_context` kind (spec 03 §1.2
/// / §4.2, T11-02). `context_version` is little-endian `u32` (matching
/// `content_blob_id`'s established version-field width); `serialization` is
/// whatever opaque bytes the caller supplies.
///
/// The *real* context-serialization format (what "context" actually contains) is
/// `[OPEN]` — spec 09 §3: "content vs context representation choice is decided by
/// the benchmark" — nothing in this codebase defines it yet. This constructor
/// only fixes the hash framing; two occurrences with distinct serializations
/// never share a subject hash (spec 03 §4.2's "context does not [share]"
/// `[FIXED]`), which is provable with any opaque serialization, real or
/// synthetic.
pub fn subject_occurrence_context(context_version: u32, serialization: &[u8]) -> String {
    hash(
        Domain::SubjectOccurrenceContext,
        &[&context_version.to_le_bytes(), serialization],
    )
}

/// `H(subject/memory_entry, memory_id, H(text))` — an `embedding_cache.subject_hash`
/// for the `memory_entry` kind (spec 03 §1.2 / §4.2, T11-02). `memory_id` is
/// hashed as its lowercase ASCII bytes (already-hex identity convention); the
/// table's own `H(text)` inner hash is computed here via
/// [`crate::hash::sha256_hex`] — the same non-domain-separated integrity-digest
/// family the vector-bytes `embedding_cache.checksum` uses, not a spec 03 §1.2
/// content-identity domain (no domain exists for raw memory text, and the memory
/// tables this would back do not exist before group 14).
///
/// Call it with the entry's own `memory_entry.text` — there is only one.
///
/// T21-02 briefly wrapped this in a store-side `memory_entry_subject_hash`
/// taking an `EffectiveText`, because ADR-0010 gave an entry two texts (its own
/// and an English variant) that three readers derived this hash from
/// independently: disagree about which text, and the dense leg silently finds
/// no vector. ADR-0011 removed the second text instead of policing it —
/// `T21-13` — so the wrapper, its private-field type and the source lint that
/// pinned this function to two call sites are all gone, and the plain form is
/// correct again by construction rather than by discipline.
pub fn subject_memory_entry(memory_id: &str, text: &str) -> String {
    let text_hash = crate::hash::sha256_hex(text.as_bytes());
    hash(
        Domain::SubjectMemoryEntry,
        &[memory_id.as_bytes(), text_hash.as_bytes()],
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
                Domain::SignatureFingerprint,
                "447e7f848f8176ebd3a0c7585eead7bc1f206914a3f48b2928b8b472b5be9b2b",
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
            (
                Domain::TruncatedExcerpt,
                "29786b15d9bea2651bdd44fb3f8de9d5fd0f09c767e67f1a943838dadd497318",
            ),
        ];
        for (domain, expected) in table {
            assert_eq!(hash(domain, GOLDEN_FIELDS), expected, "domain {domain:?}");
        }
        // Domain separation: every digest is distinct.
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
        assert_eq!(
            signature_fingerprint("fn\u{1f}foo\u{1f}(number)"),
            hash(Domain::SignatureFingerprint, &[b"fn\x1ffoo\x1f(number)"],),
        );
    }

    // ---- Typed subject-hash constructors (T11-02) --------------------------
    //
    // Real field-shape tests, distinct from `golden_hashes_for_every_domain`
    // above (which only proves domain separation of the raw enum against generic
    // placeholder fields) — these pin the actual byte layout each constructor
    // assembles ("hash golden each kind"). Golden hex constants below are pinned
    // from this constructor's own output (via `hash`/`encode`, already proven
    // against the published BLAKE3 reference vectors above), not hand-computed.

    #[test]
    fn subject_content_blob_matches_generic_single_field() {
        assert_eq!(
            subject_content_blob("blob-abc"),
            hash(Domain::SubjectContentBlob, &[b"blob-abc"]),
        );
        assert_eq!(
            subject_content_blob("blob-abc"),
            "f0634bf5ed3c9dd6d0385323512199dde25cc05fe37799795c8c2c984835cbc8",
        );
    }

    #[test]
    fn subject_content_blob_shares_across_occurrences() {
        // Two occurrences whose `parsed_unit.blob_id` coincide (structural
        // sharing, spec 06 §2) resolve to the identical subject hash.
        assert_eq!(
            subject_content_blob("shared-blob"),
            subject_content_blob("shared-blob")
        );
        assert_ne!(
            subject_content_blob("blob-a"),
            subject_content_blob("blob-b")
        );
    }

    #[test]
    fn subject_occurrence_context_matches_generic_two_fields() {
        assert_eq!(
            subject_occurrence_context(1, b"ctx-serialization"),
            hash(
                Domain::SubjectOccurrenceContext,
                &[&1u32.to_le_bytes(), b"ctx-serialization"],
            ),
        );
        assert_eq!(
            subject_occurrence_context(1, b"ctx-serialization"),
            "e78b81fe0cd199cbe131a33299e6cdc727b7b391b5c4c228240a6eb999ff7b67",
        );
    }

    #[test]
    fn subject_occurrence_context_does_not_share_across_distinct_serializations() {
        // "context does not [share]" (spec 03 §4.2 [FIXED]) — provable with any
        // opaque serialization, since the real context format is [OPEN] (09 §3).
        let a = subject_occurrence_context(1, b"occurrence-A-context");
        let b = subject_occurrence_context(1, b"occurrence-B-context");
        assert_ne!(a, b);
        // A version bump alone (same bytes) also changes the subject hash.
        assert_ne!(
            subject_occurrence_context(1, b"same-bytes"),
            subject_occurrence_context(2, b"same-bytes"),
        );
    }

    #[test]
    fn subject_memory_entry_hashes_h_of_text_as_the_second_field() {
        let text_hash = crate::hash::sha256_hex(b"remember this");
        assert_eq!(
            subject_memory_entry("memory-1", "remember this"),
            hash(
                Domain::SubjectMemoryEntry,
                &[b"memory-1", text_hash.as_bytes()],
            ),
        );
        assert_eq!(
            subject_memory_entry("memory-1", "remember this"),
            "fda86b18e0247136ff09c516dd0256a59a3b6bfaa128e60e2d2e09246194dbf8",
        );
        // Distinct text ⇒ distinct subject hash for the same memory_id.
        assert_ne!(
            subject_memory_entry("memory-1", "remember this"),
            subject_memory_entry("memory-1", "remember something else"),
        );
        // Distinct memory_id ⇒ distinct subject hash for the same text.
        assert_ne!(
            subject_memory_entry("memory-1", "remember this"),
            subject_memory_entry("memory-2", "remember this"),
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
