//! The `signature_fingerprint` descriptor and hash (ADR-0002; spec 03 §2.4).
//!
//! The `sig` field of a `SyntaxLocator` is a domain-separated BLAKE3 hash
//! ([`local_rag_core::identity::domain::signature_fingerprint`]) of a **canonical,
//! deterministic descriptor** of a unit's signature, assembled from the parse
//! subtree only — never from a path or byte offset. Hashing makes `sig`
//! delimiter-safe by construction (64 lowercase hex, no `;`/`=`), so the locator
//! serialization needs no escaping.
//!
//! The descriptor is built as an ordered list of fields joined by the ASCII Unit
//! Separator ([`FIELD_SEP`]) and hashed as one opaque domain field, so its
//! internal shape may evolve within a `queries=`/`grammar=` rebuild event
//! (spec 03 §2.3.1) without a `HASH_SCHEMA_VERSION` bump. Per-language adapters
//! decide which fields a descriptor carries; this module owns only the framing.

use local_rag_core::identity::domain;

/// The field separator inside a descriptor (ASCII Unit Separator, `U+001F`).
/// Chosen because it does not occur in real source identifiers/type text.
pub const FIELD_SEP: char = '\u{1f}';

/// Accumulates the ordered fields of a signature descriptor.
#[derive(Debug, Clone)]
pub struct SignatureDescriptor {
    fields: Vec<String>,
}

impl SignatureDescriptor {
    /// Start a descriptor with the always-present head fields: the language id,
    /// the schema-level unit kind, and the language-level kind label.
    pub fn new(language: &str, unit_kind: &str, lang_kind: &str) -> Self {
        Self {
            fields: vec![
                language.to_string(),
                unit_kind.to_string(),
                lang_kind.to_string(),
            ],
        }
    }

    /// Append one structural field (in a fixed, per-language order).
    pub fn push(&mut self, field: impl AsRef<str>) {
        self.fields.push(field.as_ref().to_string());
    }

    /// The canonical descriptor string (fields joined by [`FIELD_SEP`]).
    pub fn canonical(&self) -> String {
        // `char::to_string` then join keeps this allocation-simple and explicit.
        self.fields.join(&FIELD_SEP.to_string())
    }

    /// The `sig` value: the domain-separated hash of [`canonical`](Self::canonical).
    pub fn fingerprint(&self) -> String {
        fingerprint(&self.canonical())
    }
}

/// Domain-separated fingerprint of an already-assembled canonical descriptor.
pub fn fingerprint(canonical_descriptor: &str) -> String {
    domain::signature_fingerprint(canonical_descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_is_order_sensitive_and_hex() {
        let mut a = SignatureDescriptor::new("typescript", "symbol", "function");
        a.push("foo");
        a.push("(a: number)");
        let mut b = SignatureDescriptor::new("typescript", "symbol", "function");
        b.push("foo");
        b.push("(a: string)");
        let fa = a.fingerprint();
        let fb = b.fingerprint();
        assert_ne!(fa, fb, "different params ⇒ different sig");
        assert_eq!(fa.len(), 64);
        assert!(
            fa.bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
        assert!(!fa.contains(';') && !fa.contains('='));
    }

    #[test]
    fn identical_descriptors_match() {
        let mut a = SignatureDescriptor::new("typescript", "symbol", "class");
        a.push("Foo");
        let mut b = SignatureDescriptor::new("typescript", "symbol", "class");
        b.push("Foo");
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn canonical_uses_unit_separator() {
        let d = SignatureDescriptor::new("typescript", "file", "file");
        assert_eq!(d.canonical(), "typescript\u{1f}file\u{1f}file");
    }
}
