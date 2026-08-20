//! Scalar `ToSql` / `FromSql` impls for the Turso backend.
//!
//! `Nullable<T>` falls out from diesel's `Option<T>` blanket impls once
//! `push_null_value` works (see `bind.rs`). Date/Time/Timestamp use the
//! chrono crate via the optional `turso-chrono` feature — see
//! `super::chrono`.

use crate::deserialize::{self, FromSql};
use crate::query_builder::QueryId;
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types;

use crate::turso::backend::Turso;
use crate::turso::value::{mismatch, TursoValue};

// Widening integer codecs share the same shape: emit Value::Integer on
// encode, narrow-and-validate on decode.
macro_rules! int_codec {
    ($sql_ty:ty, $rust_ty:ty) => {
        impl ToSql<$sql_ty, Turso> for $rust_ty {
            fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
                out.set_value(*self);
                Ok(IsNull::No)
            }
        }
        impl FromSql<$sql_ty, Turso> for $rust_ty {
            fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
                match v.as_turso() {
                    turso::Value::Integer(i) => <$rust_ty>::try_from(*i).map_err(|_| {
                        format!("integer {i} out of range for {}", stringify!($rust_ty),).into()
                    }),
                    other => mismatch(stringify!($rust_ty), other),
                }
            }
        }
    };
}
int_codec!(sql_types::SmallInt, i16);
int_codec!(sql_types::Integer, i32);

// i64 ↔ BigInt: no narrowing needed on the FromSql path.
impl ToSql<sql_types::BigInt, Turso> for i64 {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(*self);
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::BigInt, Turso> for i64 {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            turso::Value::Integer(i) => Ok(*i),
            other => mismatch("Integer", other),
        }
    }
}

impl ToSql<sql_types::Bool, Turso> for bool {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(*self);
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::Bool, Turso> for bool {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            turso::Value::Integer(i) => Ok(*i != 0),
            other => mismatch("Integer(0|1)", other),
        }
    }
}

// Real types accept either Real or Integer on read — turso will hand back
// whatever serial type fits, so we tolerate both.
macro_rules! real_codec {
    ($sql_ty:ty, $rust_ty:ty) => {
        impl ToSql<$sql_ty, Turso> for $rust_ty {
            fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
                out.set_value(*self);
                Ok(IsNull::No)
            }
        }
        impl FromSql<$sql_ty, Turso> for $rust_ty {
            fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
                match v.as_turso() {
                    turso::Value::Real(f) => Ok(*f as $rust_ty),
                    turso::Value::Integer(i) => Ok(*i as $rust_ty),
                    other => mismatch(stringify!($rust_ty), other),
                }
            }
        }
    };
}
real_codec!(sql_types::Float, f32);
real_codec!(sql_types::Double, f64);

// Diesel provides blanket `ToSql<Text, DB> for String` forwarding to `str`,
// so we only impl the `str` side.
impl ToSql<sql_types::Text, Turso> for str {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self);
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::Text, Turso> for String {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            // The row owns this and outlives the call, so the clone is
            // forced: `FromSql` is handed a borrow and has to return a value
            // that owns its buffer. Nothing here can hand the row's `String`
            // over instead — `Row::get` takes `&self`. A target that does not
            // need to own the bytes should not come through here at all; see
            // the `SharedString` impl below.
            turso::Value::Text(s) => Ok(s.clone()),
            other => mismatch("Text", other),
        }
    }
}

/// `gpui::SharedString` off a Turso `TEXT` column, without the `String` in
/// the middle.
///
/// A `SharedString` column used to cost three allocations to read: turso's
/// `get_value` building the row's `String`, `FromSql<Text, Turso> for
/// String` cloning it, and `SharedString` copying out of the clone because
/// it can never adopt a `String`'s buffer. Only the first is real work — so
/// this skips the other two and reads the row's `&str` directly. Where
/// `SharedString` is backed by a `SmolStr`, anything under 23 bytes is then
/// stored inline and the read allocates nothing at all.
///
/// Measured on a counting allocator, per decoded column, against a
/// `SmolStr`-backed `SharedString`: a 13-byte value went from 1 allocation
/// and 13 bytes to 0 and 0, and a 60-byte value from 2 and 140 to 1 and 80.
///
/// This does not overlap the generic impl in
/// `crate::type_impls::primitives`, which is bounded on
/// `*const str: FromSql<ST, DB>` — a bound Turso deliberately does not
/// satisfy, since its raw value is a tagged `turso::Value` and not a byte
/// slice to be re-decoded.
#[cfg(feature = "gpui")]
impl FromSql<sql_types::Text, Turso> for gpui::SharedString {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            turso::Value::Text(s) => Ok(gpui::SharedString::new(s.as_str())),
            other => mismatch("Text", other),
        }
    }
}

// Same story for Vec<u8>: diesel has a `Vec<u8>` blanket forwarding to `[u8]`.
impl ToSql<sql_types::Binary, Turso> for [u8] {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self);
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::Binary, Turso> for Vec<u8> {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            turso::Value::Blob(b) => Ok(b.clone()),
            other => mismatch("Blob", other),
        }
    }
}

/// The SQL type for a timestamp carrying a UTC offset.
///
/// Stored as ISO-8601 `TEXT`, because Turso is a STRICT-table engine and
/// there is no wider storage class to put an offset in. Turso's own
/// timestamp functions read the same spelling.
///
/// This is Turso's, not PostgreSQL's. It has to be: the `AsExpression`
/// impls that let a `chrono::DateTime<Utc>` be bound against it are impls
/// of a diesel trait for a chrono type, which only diesel itself can write.
/// The out-of-tree arrangement this replaced could not, so it enabled
/// diesel's `postgres_backend` feature purely to borrow
/// `sql_types::Timestamptz` and the impls that came with it — dragging a
/// whole second backend into every build that wanted a timezone-aware
/// column. See `crate::type_impls::date_and_time` for where the impls are
/// now written.
#[derive(Debug, Clone, Copy, Default, QueryId, crate::sql_types::SqlType)]
pub struct Timestamptz;

// `table!` emits the `std::ops::Add`/`Sub` impls for every column it
// declares, and those name `<SqlType as sql_types::ops::Add>::Rhs` — so a
// SQL type without these impls cannot appear in a `table!` at all. Turso's
// `Timestamptz` had neither, which is why the one test that declares a
// `Timestamptz` column has never compiled: the whole `turso` test binary
// failed to build on it, and a test binary that does not build reports no
// failures. Mirrors `Timestamp`'s pair in `sql_types::ops`.
impl crate::sql_types::ops::Add for Timestamptz {
    type Rhs = crate::sql_types::Interval;
    type Output = Timestamptz;
}

impl crate::sql_types::ops::Sub for Timestamptz {
    type Rhs = crate::sql_types::Interval;
    type Output = Timestamptz;
}
