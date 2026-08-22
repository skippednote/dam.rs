//! A wrapper whose formatting cannot reveal its contents.
//!
//! Config values, error payloads, and span attributes all end up somewhere they
//! can be read — stdout, a log aggregator, an OTel collector. Remembering to
//! redact at each of those call sites does not scale, so the value itself refuses
//! to render. `expose()` is the one deliberate way out, and it greps well.

use std::fmt;

/// Holds a sensitive value. `Debug`, `Display`, and `Serialize` all render
/// `[REDACTED]` rather than the contents.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    /// Yields the underlying value. Named to be conspicuous in review and in
    /// `grep` — every call is a deliberate decision to handle plaintext.
    pub fn expose(&self) -> &T {
        &self.0
    }

    /// Consumes the wrapper, yielding the value.
    pub fn into_inner(self) -> T {
        self.0
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Secret<U> {
        Secret(f(self.0))
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl<T> From<T> for Secret<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

/// Serialises as `[REDACTED]`.
///
/// This is deliberately lossy. A `Secret` must never round-trip through
/// serialisation, because the places we serialise to — logs, API responses,
/// telemetry — are exactly the places the value must not appear. Config
/// *deserialises* into a `Secret` (see below); it never serialises back out.
impl<T> serde::Serialize for Secret<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str("[REDACTED]")
    }
}

impl<'de, T: serde::Deserialize<'de>> serde::Deserialize<'de> for Secret<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_preserves_redaction() {
        let s = Secret::new("abc".to_owned()).map(|v| v.len());
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(*s.expose(), 3);
    }

    #[test]
    fn equality_works_without_exposing() {
        assert_eq!(Secret::new(1), Secret::new(1));
        assert_ne!(Secret::new(1), Secret::new(2));
    }
}
