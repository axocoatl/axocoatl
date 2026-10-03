//! The check that a credentialed route's responses do not carry the
//! credential back into the container.
//!
//! The scan is an exact-match search of the response headers, body and
//! trailers for each needle: the credential itself and, for basic
//! credentials, the base64 `username:credential` the request carried. The
//! body is searched as a stream: the last `longest needle - 1` bytes are
//! held back from every chunk until the next one arrives, so no byte that
//! could start a match is ever passed on. A match stops the response.
//!
//! The search sees bytes as the upstream sent them, so a credentialed route
//! refuses compressed responses unless `allow_encoded_responses` is set.

use bytes::{Bytes, BytesMut};
use zeroize::Zeroizing;

/// The response carried a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the response carried the route's credential")]
pub struct Reflected;

/// A streaming search for a route's credential. See the module docs.
pub struct ReflectionScanner {
    needles: Vec<Zeroizing<Vec<u8>>>,
    keep: usize,
    held: BytesMut,
}

impl std::fmt::Debug for ReflectionScanner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReflectionScanner")
            .field("needles", &self.needles.len())
            .field("held", &self.held.len())
            .finish()
    }
}

impl ReflectionScanner {
    /// A scanner for `needles`; empty needles are ignored. `None` when none
    /// is left.
    pub fn new(needles: Vec<Zeroizing<Vec<u8>>>) -> Option<Self> {
        let needles: Vec<_> = needles.into_iter().filter(|n| !n.is_empty()).collect();
        let longest = needles.iter().map(|needle| needle.len()).max()?;
        Some(Self {
            needles,
            keep: longest - 1,
            held: BytesMut::new(),
        })
    }

    /// Whether `haystack` contains any needle.
    pub fn contains(&self, haystack: &[u8]) -> bool {
        self.needles
            .iter()
            .any(|needle| memchr::memmem::find(haystack, needle).is_some())
    }

    /// Feed one chunk. Returns the bytes that are safe to pass on, which
    /// may be fewer than were fed, or [`Reflected`] when the bytes held and
    /// fed so far contain a needle.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Bytes, Reflected> {
        self.held.extend_from_slice(chunk);
        if self.contains(&self.held) {
            self.held.clear();
            return Err(Reflected);
        }
        let release = self.held.len().saturating_sub(self.keep);
        Ok(self.held.split_to(release).freeze())
    }

    /// The bytes still held at the end of the body; none of them starts a
    /// match, since the stream ended.
    pub fn finish(&mut self) -> Bytes {
        self.held.split().freeze()
    }

    /// Bytes held back, not yet passed on.
    pub fn held(&self) -> usize {
        self.held.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scanner(needles: &[&str]) -> ReflectionScanner {
        ReflectionScanner::new(
            needles
                .iter()
                .map(|needle| Zeroizing::new(needle.as_bytes().to_vec()))
                .collect(),
        )
        .unwrap()
    }

    /// Feed `chunks`; returns what was passed on and whether a match
    /// stopped it.
    fn run(scanner: &mut ReflectionScanner, chunks: &[&str]) -> (String, bool) {
        let mut out = Vec::new();
        for chunk in chunks {
            match scanner.push(chunk.as_bytes()) {
                Ok(bytes) => out.extend_from_slice(&bytes),
                Err(Reflected) => return (String::from_utf8(out).unwrap(), true),
            }
        }
        out.extend_from_slice(&scanner.finish());
        (String::from_utf8(out).unwrap(), false)
    }

    #[test]
    fn no_needle_no_scanner() {
        assert!(ReflectionScanner::new(Vec::new()).is_none());
        assert!(ReflectionScanner::new(vec![Zeroizing::new(Vec::new())]).is_none());
    }

    #[test]
    fn clean_bodies_pass_whole_and_in_order() {
        let mut s = scanner(&["SECRET-123"]);
        let (out, stopped) = run(
            &mut s,
            &["hello ", "world, ", "", "SECRET-12", "4 is not it"],
        );
        assert!(!stopped);
        assert_eq!(out, "hello world, SECRET-124 is not it");
    }

    #[test]
    fn a_match_split_across_chunks_is_stopped_before_any_of_it_passes() {
        for split in 1..10 {
            let secret = "SECRET-123";
            let body = format!("prefix-{secret}-suffix");
            let at = "prefix-".len() + split;
            let mut s = scanner(&[secret]);
            let (out, stopped) = run(&mut s, &[&body[..at], &body[at..]]);
            assert!(stopped, "split {split}");
            assert!(!out.contains('S'), "split {split}: passed {out:?}");
            assert!("prefix-".starts_with(&out), "split {split}: passed {out:?}");
        }
        // One byte at a time.
        let mut s = scanner(&["SECRET-123"]);
        let body = "aaaaSECRET-123bbbb";
        let chunks: Vec<String> = body.chars().map(String::from).collect();
        let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let (out, stopped) = run(&mut s, &chunks);
        assert!(stopped);
        assert_eq!(out, "aaaa");
    }

    #[test]
    fn any_needle_matches_and_the_hold_is_the_longest_less_one() {
        let mut s = scanner(&["abc", "dGVzdDpzZWNyZXQ="]);
        assert_eq!(s.push(b"0123456789abcdefghij").unwrap_err(), Reflected);
        let mut s = scanner(&["abc", "dGVzdDpzZWNyZXQ="]);
        let passed = s.push(b"0123456789xyzxyzxyzx").unwrap();
        assert_eq!(passed.len(), 20 - 15);
        assert_eq!(s.held(), 15);
        assert!(s.push(b"..dGVzdDpzZWNyZXQ=").is_err());
    }
}
