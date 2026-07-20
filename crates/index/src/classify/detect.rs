//! Content-based skip detectors: binary, Git-LFS pointer, and UTF-8 validity
//! (spec 06 §2.2). Each is a pure function over bytes/path so the classifier and
//! (later) the reconcile tree scan share one deterministic definition.

/// How many leading bytes the binary NUL heuristic inspects.
///
/// A NUL byte anywhere in this prefix marks the content binary; scanning a bounded
/// prefix keeps classification cheap on large files.
pub const BINARY_SNIFF_LEN: usize = 8192;

/// File extensions always treated as binary regardless of content (spec 06 §2.2
/// "NUL heuristic + extension list"). Lowercase, without the leading dot.
///
/// Curated and intentionally conservative; extends over time. Matched against the
/// final `.`-delimited suffix of the path's last component.
pub const BINARY_EXTENSIONS: &[&str] = &[
    // images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "tif", "heic", "avif",
    // audio / video
    "mp3", "wav", "flac", "ogg", "aac", "mp4", "m4a", "m4v", "avi", "mov", "mkv", "webm", "wmv",
    // archives / compression
    "zip", "gz", "tgz", "bz2", "xz", "zst", "7z", "rar", "tar", "lz4", // documents
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", // fonts
    "woff", "woff2", "ttf", "otf", "eot", // executables / objects / libraries
    "exe", "dll", "so", "dylib", "a", "o", "obj", "lib", "class", "jar", "wasm", "pyc", "pyd",
    // data stores / blobs
    "sqlite", "db", "bin", "dat", "pack", "idx",
];

/// Whether `content` looks binary: a NUL byte in the first [`BINARY_SNIFF_LEN`]
/// bytes, or a path whose extension is in [`BINARY_EXTENSIONS`].
pub fn is_binary(path: &str, content: &[u8]) -> bool {
    if has_binary_extension(path) {
        return true;
    }
    let sniff = &content[..content.len().min(BINARY_SNIFF_LEN)];
    sniff.contains(&0u8)
}

/// Whether `path`'s extension is in [`BINARY_EXTENSIONS`] (case-insensitive).
pub fn has_binary_extension(path: &str) -> bool {
    match extension_of(path) {
        Some(ext) => {
            let lower = ext.to_ascii_lowercase();
            BINARY_EXTENSIONS.contains(&lower.as_str())
        }
        None => false,
    }
}

/// The extension of `path`'s final component (text after its last `.`), or `None`
/// when the component has no extension (no dot, a leading-dot dotfile, or a
/// trailing dot).
fn extension_of(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let dot = name.rfind('.')?;
    // A leading dot (`.gitignore`) or trailing dot (`foo.`) is not an extension.
    if dot == 0 || dot + 1 == name.len() {
        return None;
    }
    Some(&name[dot + 1..])
}

/// The canonical first line of a Git-LFS pointer file (spec 06 §2.2 "pointer
/// file"). The version URL is the stable v1 spec identifier.
const LFS_VERSION_LINE: &str = "version https://git-lfs.github.com/spec/v1";

/// Whether `content` is a Git-LFS pointer file.
///
/// A pointer is a small UTF-8 text blob whose first line is the LFS v1 version
/// line and which carries the required `oid sha256:` and `size` entries. Detected
/// by format, not by extension, so it is caught even for a path like `asset.png`.
pub fn is_lfs_pointer(content: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(content) else {
        return false;
    };
    let mut lines = text.lines();
    match lines.next() {
        Some(first) => {
            if first.trim_end() != LFS_VERSION_LINE {
                return false;
            }
        }
        None => return false,
    }
    let has_oid = text
        .lines()
        .any(|l| l.starts_with("oid sha256:") && l.len() > "oid sha256:".len());
    let has_size = text
        .lines()
        .any(|l| l.starts_with("size ") && l["size ".len()..].trim().parse::<u64>().is_ok());
    has_oid && has_size
}

/// Whether `content` decodes as valid UTF-8.
///
/// T03-02's `encoding` gate: v0 supports only UTF-8, so invalid bytes ⇒
/// `skipped_file(reason='encoding')` (no transcoding without an offset mapping,
/// spec 03 §2.3.1 / 06 §2.1 `[FIXED]`). Full `source_encoding`/`newline_style`
/// detection for *accepted* files is T03-03, not here.
pub fn is_valid_utf8(content: &[u8]) -> bool {
    std::str::from_utf8(content).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_by_nul_byte() {
        assert!(is_binary("src/x.rs", b"abc\0def"));
        assert!(!is_binary("src/x.rs", b"abcdef\n"));
    }

    #[test]
    fn binary_by_extension_without_nul() {
        assert!(is_binary("assets/logo.png", b"not really png bytes"));
        assert!(is_binary("A/B/PHOTO.JPG", b"text"));
        assert!(!is_binary("src/main.rs", b"fn main() {}"));
    }

    #[test]
    fn extension_edge_cases() {
        assert_eq!(extension_of("a/b/c.rs"), Some("rs"));
        assert_eq!(extension_of("Makefile"), None);
        assert_eq!(extension_of(".gitignore"), None);
        assert_eq!(extension_of("archive.tar.gz"), Some("gz"));
        assert_eq!(extension_of("trailing."), None);
    }

    #[test]
    fn lfs_pointer_recognized_and_near_miss_rejected() {
        let ptr = "version https://git-lfs.github.com/spec/v1\n\
                   oid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\n\
                   size 12345\n";
        assert!(is_lfs_pointer(ptr.as_bytes()));

        // Missing oid / size, or wrong first line: not a pointer.
        assert!(!is_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\nsize 5\n"
        ));
        assert!(!is_lfs_pointer(b"just some text\noid sha256:x\nsize 5\n"));
        assert!(!is_lfs_pointer(b""));
    }

    #[test]
    fn utf8_validity_gate() {
        assert!(is_valid_utf8("héllo".as_bytes()));
        // Lone continuation byte: invalid UTF-8, but contains no NUL.
        assert!(!is_valid_utf8(&[0xFF, 0xFE, 0x41]));
        assert!(![0xFFu8, 0xFE, 0x41].contains(&0u8));
    }
}
