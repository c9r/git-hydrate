//! The pointer file that git stores in place of a tracked file.
//!
//! The format is git-lfs's pointer format, so a repository's pointers are
//! legible to git-lfs and to every tool that reads them. A pointer names
//! the sha256 of the object and its size, in three lines.

/// The version line every pointer starts with.
pub const VERSION_URL: &str = "https://git-lfs.github.com/spec/v1";

/// The version line git-lfs wrote before it was renamed. Accepted on parse, never emitted.
const LEGACY_VERSION_URL: &str = "https://hawser.github.com/spec/v1";

/// A pointer file, extension lines included, is shorter than this many bytes.
/// Content at least this long is never a pointer, so the clean filter can
/// decide what it is looking at from the first kilobyte.
pub const MAX_LEN: usize = 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pointer {
    /// The object's sha256 as lowercase hex.
    pub oid: String,
    /// The object's size in bytes.
    pub size: u64,
}

impl Pointer {
    /// Parses a pointer file. Returns `None` for anything that is not one.
    ///
    /// The parse is strict about what a pointer must contain and lenient about
    /// key order, so pointers written by other tools are recognized whatever
    /// order they chose. Extension lines are accepted and ignored.
    pub fn parse(content: &[u8]) -> Option<Pointer> {
        if content.is_empty() || content.len() >= MAX_LEN {
            return None;
        }
        let text = std::str::from_utf8(content).ok()?;
        if !text.ends_with('\n') {
            return None;
        }
        let mut lines = text.lines();
        let url = lines.next()?.strip_prefix("version ")?;
        if url != VERSION_URL && url != LEGACY_VERSION_URL {
            return None;
        }
        let mut oid = None;
        let mut size = None;
        for line in lines {
            let (key, value) = line.split_once(' ')?;
            if key.is_empty() || value.is_empty() || value.starts_with(' ') {
                return None;
            }
            match key {
                "oid" => {
                    if oid.is_some() {
                        return None;
                    }
                    let hex = value.strip_prefix("sha256:")?;
                    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                        return None;
                    }
                    oid = Some(hex.to_string());
                }
                "size" => {
                    if size.is_some() || !value.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    size = Some(value.parse().ok()?);
                }
                "version" => return None,
                _ => {}
            }
        }
        Some(Pointer { oid: oid?, size: size? })
    }

    /// Whether the content is a pointer file.
    pub fn is_pointer(content: &[u8]) -> bool {
        Pointer::parse(content).is_some()
    }

    /// The pointer file's bytes, in the canonical form git-lfs specifies.
    pub fn to_bytes(&self) -> Vec<u8> {
        format!("version {VERSION_URL}\noid sha256:{}\nsize {}\n", self.oid, self.size).into_bytes()
    }
}

/// Whether a string is a lowercase hex sha256.
pub fn is_oid(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OID: &str = "4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393";

    fn pointer() -> Pointer {
        Pointer { oid: OID.to_string(), size: 12345 }
    }

    #[test]
    fn round_trip() {
        let bytes = pointer().to_bytes();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            format!("version {VERSION_URL}\noid sha256:{OID}\nsize 12345\n")
        );
        assert_eq!(Pointer::parse(&bytes), Some(pointer()));
    }

    #[test]
    fn accepts_extension_lines_and_the_legacy_url() {
        let text = format!("version {LEGACY_VERSION_URL}\next-0-foo sha256:{OID}\noid sha256:{OID}\nsize 1\n");
        assert_eq!(Pointer::parse(text.as_bytes()), Some(Pointer { oid: OID.to_string(), size: 1 }));
    }

    #[test]
    fn rejects_what_is_not_a_pointer() {
        assert!(Pointer::parse(b"").is_none());
        assert!(Pointer::parse(b"hello\n").is_none());
        assert!(Pointer::parse(format!("version {VERSION_URL}\noid sha256:{OID}\nsize 12345").as_bytes()).is_none());
        assert!(Pointer::parse(format!("version {VERSION_URL}\noid sha256:{OID}\n").as_bytes()).is_none());
        assert!(Pointer::parse(format!("version {VERSION_URL}\nsize 5\n").as_bytes()).is_none());
        assert!(
            Pointer::parse(format!("version {VERSION_URL}\noid sha256:{}\nsize 5\n", OID.to_uppercase()).as_bytes())
                .is_none()
        );
        assert!(Pointer::parse(format!("version {VERSION_URL}\noid sha256:{OID}\nsize +5\n").as_bytes()).is_none());
        assert!(
            Pointer::parse(format!("version {VERSION_URL}\noid sha256:{OID}\noid sha256:{OID}\nsize 5\n").as_bytes())
                .is_none()
        );
        assert!(
            Pointer::parse(format!("version https://example.com/v9\noid sha256:{OID}\nsize 5\n").as_bytes()).is_none()
        );
        let long = format!("version {VERSION_URL}\noid sha256:{OID}\nsize 5\nz {}\n", "x".repeat(MAX_LEN));
        assert!(Pointer::parse(long.as_bytes()).is_none());
    }
}
