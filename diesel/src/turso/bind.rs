//! `TursoBindCollector` — receives the bind parameter stream from diesel's
//! query builder and produces a `Vec<turso::Value>` that we hand to
//! `turso::Connection::{execute, query}`.
//!
//! Pattern mirrors diesel's SQLite backend: each bind is funnelled through
//! an owned `TursoBindBuffer` that `ToSql` impls populate via
//! `Output::set_value(T)` where `T: Into<TursoBindBuffer>`.

use std::marker::PhantomData;

use crate::backend::Backend;
use crate::query_builder::BindCollector;
use crate::result::{Error as DieselError, QueryResult};
use crate::serialize::{IsNull, Output, ToSql};
use crate::sql_types::HasSqlType;

use crate::turso::backend::{Turso, TursoType};

#[derive(Default, Debug)]
pub struct TursoBindCollector<'a> {
    binds: Vec<turso::Value>,
    _lt: PhantomData<&'a ()>,
}

impl<'a> TursoBindCollector<'a> {
    /// Drain the collected binds and hand back the value list in order —
    /// exactly what `turso::Connection::{execute,query}(sql, params)` wants.
    ///
    /// `pub` rather than `pub(crate)` because the only caller is the
    /// connection, and the connection is in `diesel-async` (see the module
    /// docs on [`crate::turso`]).
    pub fn into_values(self) -> Vec<turso::Value> {
        self.binds
    }
}

/// The per-bind scratchpad handed to `ToSql` impls through `Output`.
/// Populated via `From<T>` conversions for every Rust type we accept.
#[derive(Debug)]
pub struct TursoBindBuffer {
    pub(crate) inner: turso::Value,
}

impl Default for TursoBindBuffer {
    fn default() -> Self {
        Self {
            inner: turso::Value::Null,
        }
    }
}

impl TursoBindBuffer {
    /// The one value a `ToSql` impl put here.
    ///
    /// Exists because a composite field is encoded by running `to_sql`
    /// into a throwaway buffer and taking what lands — see
    /// [`encode_field`](crate::turso::union::encode_field). That shortcut is only
    /// available because this buffer *is* a single `turso::Value` rather
    /// than a byte stream a backend would have to frame.
    pub(crate) fn into_value(self) -> turso::Value {
        self.inner
    }
}

// -- From impls wiring ToSql bodies to turso's Value variants. ---------------

macro_rules! from_integer {
    ($ty:ty) => {
        impl From<$ty> for TursoBindBuffer {
            fn from(v: $ty) -> Self {
                Self {
                    inner: turso::Value::Integer(v as i64),
                }
            }
        }
    };
}
from_integer!(i16);
from_integer!(i32);
from_integer!(i64);

impl From<bool> for TursoBindBuffer {
    fn from(b: bool) -> Self {
        Self {
            inner: turso::Value::Integer(i64::from(b)),
        }
    }
}

impl From<f32> for TursoBindBuffer {
    fn from(f: f32) -> Self {
        Self {
            inner: turso::Value::Real(f64::from(f)),
        }
    }
}

impl From<f64> for TursoBindBuffer {
    fn from(f: f64) -> Self {
        Self {
            inner: turso::Value::Real(f),
        }
    }
}

impl From<String> for TursoBindBuffer {
    fn from(s: String) -> Self {
        Self {
            inner: turso::Value::Text(s),
        }
    }
}

impl From<&str> for TursoBindBuffer {
    fn from(s: &str) -> Self {
        Self {
            inner: turso::Value::Text(s.to_string()),
        }
    }
}

impl From<Vec<u8>> for TursoBindBuffer {
    fn from(b: Vec<u8>) -> Self {
        Self {
            inner: turso::Value::Blob(b),
        }
    }
}

impl From<&[u8]> for TursoBindBuffer {
    fn from(b: &[u8]) -> Self {
        Self {
            inner: turso::Value::Blob(b.to_vec()),
        }
    }
}

// -- BindCollector impl ------------------------------------------------------

impl<'a> BindCollector<'a, Turso> for TursoBindCollector<'a> {
    type Buffer = TursoBindBuffer;

    fn push_bound_value<T, U>(
        &mut self,
        bind: &'a U,
        metadata_lookup: &mut <Turso as crate::sql_types::TypeMetadata>::MetadataLookup,
    ) -> QueryResult<()>
    where
        Turso: Backend + HasSqlType<T>,
        U: ToSql<T, Turso> + ?Sized + 'a,
    {
        let buffer = TursoBindBuffer::default();
        let mut out = Output::new(buffer, metadata_lookup);
        let is_null = bind
            .to_sql(&mut out)
            .map_err(DieselError::SerializationError)?;
        let buffer = out.into_inner();
        self.binds.push(match is_null {
            IsNull::No => buffer.inner,
            IsNull::Yes => turso::Value::Null,
        });
        Ok(())
    }

    fn push_null_value(&mut self, _metadata: TursoType) -> QueryResult<()> {
        self.binds.push(turso::Value::Null);
        Ok(())
    }
}
