//! `TursoValue` — the `Backend::RawValue` that `FromSql` impls receive.
//!
//! Since turso already hands back a tagged `turso::Value`, there's no
//! byte-slice re-decoding; we just hand out a borrow.

use crate::deserialize;

/// View into a single cell of a query result.
#[derive(Debug, Clone, Copy)]
pub struct TursoValue<'a> {
    inner: &'a turso::Value,
}

impl<'a> TursoValue<'a> {
    /// Wrap a borrowed value so a `FromSql` impl can be called on it.
    ///
    /// Public because composite field decoding runs `FromSql` against a
    /// value pulled out of a record rather than out of a result row — see
    /// [`decode_field`](crate::turso::union::decode_field).
    pub fn new(inner: &'a turso::Value) -> Self {
        Self { inner }
    }

    /// Access the underlying turso value directly.
    pub fn as_turso(&self) -> &'a turso::Value {
        self.inner
    }
}

/// Shared helper: produce a "expected X, got {got:?}" deserialize error.
pub(crate) fn mismatch<T>(expected: &'static str, got: &turso::Value) -> deserialize::Result<T> {
    Err(format!("expected {expected}, got {got:?}").into())
}
