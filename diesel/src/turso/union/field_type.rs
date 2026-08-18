//! Which SQL type a Rust type takes when it appears as a field inside a
//! composite, and the two-line bridge from there to diesel's own
//! `ToSql`/`FromSql`.
//!
//! # What this replaced, and why
//!
//! There used to be a `FieldCodec` trait here: 269 lines implementing
//! `into_value` / `from_value` for `i16`, `i32`, `i64`, `bool`, `f32`,
//! `f64`, `String`, `Vec<u8>`, `Uuid`, `SharedString`, four chrono types
//! and `Option<T>` — every one of which *already* had a
//! `ToSql`/`FromSql` pair for `Turso` making the identical decision. Two
//! conversion layers over one set of types, so a type that worked as a
//! column did not work as a composite field until somebody wrote the
//! second impl, and three crates outside turbo-diesel carried a hand-written
//! `FieldCodec` whose only content was "same as my `ToSql`".
//!
//! What genuinely had to survive that deletion is the *choice*: a Rust
//! type maps to many SQL types (`i64` is `BigInt` here and could be
//! `Timestamp` elsewhere) and diesel deliberately does not guess. So the
//! choice lives in [`TursoFieldType`] — one associated type per Rust type,
//! no conversion code — and the conversion itself is `ToSql`/`FromSql`,
//! reached through [`encode_field`] / [`decode_field`].
//!
//! That works on this backend for a reason worth naming: `TursoBindBuffer`
//! holds exactly one `turso::Value` and `TursoValue` is a borrow of one, so
//! "encode a field" really is `Output::new` plus `to_sql`, with no byte
//! buffer to frame and no length prefix to invent. Turso's
//! `MetadataLookup` is `()`, so there is nothing to thread through either.
//! On a backend whose bind buffer is a byte stream this shortcut would not
//! exist.
//!
//! The old `FieldCodec::sql_type()` also returned a `&'static str` DDL
//! keyword, which made the DDL's notion of a field's type a *third*
//! mechanism unrelated to `HasSqlType`. [`ddl_type_name`] deletes that
//! third mechanism: the keyword is derived from the SQL type's own
//! `HasSqlType<_> for Turso` metadata, so a field's storage class in
//! `CREATE TYPE` and its storage class as a bind value cannot disagree.

use std::error::Error;

use crate::deserialize::FromSql;
use crate::serialize::{IsNull, Output, ToSql};
use crate::sql_types::{HasSqlType, Nullable};

use crate::turso::backend::{Turso, TursoType};
use crate::turso::bind::TursoBindBuffer;
use crate::turso::value::TursoValue;

/// Anything that can be a field of a `#[derive(UnionSchema)]` STRUCT
/// variant carries the SQL type it is stored as.
///
/// This is a defaulting mechanism, not a constraint: a field may override
/// it with `#[union(sql_type = …)]` when the default is not what that
/// column wants. Implement it for a newtype in one line —
/// `impl TursoFieldType for ChatId { type SqlType = BigInt; }` — beside
/// the `ToSql`/`FromSql` pair that does the actual work.
pub trait TursoFieldType {
    type SqlType;
}

/// The Turso STRICT storage-class keyword for a SQL type, as it appears in
/// `CREATE TYPE … AS STRUCT(field <keyword>, …)`.
///
/// Read off `HasSqlType` rather than declared separately, so the DDL can
/// only ever name the class the bind path actually produces.
pub fn ddl_type_name<ST>() -> &'static str
where
    Turso: HasSqlType<ST>,
{
    match <Turso as HasSqlType<ST>>::metadata(&mut ()) {
        TursoType::Integer => "INT",
        TursoType::Real => "REAL",
        TursoType::Text => "TEXT",
        TursoType::Binary => "BLOB",
        // Nothing in the standard mapping reports `Null`, but a custom SQL
        // type could. `ANY` is the STRICT class that accepts every storage
        // class, which is the honest declaration for "unknown".
        TursoType::Null => "ANY",
    }
}

/// Lower one field value to the `turso::Value` that goes into the
/// composite's SQLite record.
///
/// `IsNull::Yes` — what diesel's blanket `ToSql<Nullable<ST>, DB> for
/// Option<T>` returns for `None` without touching the buffer — becomes
/// `Value::Null`, matching `push_bound_value`'s handling of the same case
/// for ordinary binds.
pub fn encode_field<ST, T>(value: &T) -> Result<turso::Value, Box<dyn Error + Send + Sync>>
where
    T: ToSql<ST, Turso> + ?Sized,
{
    let mut lookup = ();
    let mut out = Output::new(TursoBindBuffer::default(), &mut lookup);
    match value.to_sql(&mut out)? {
        IsNull::Yes => Ok(turso::Value::Null),
        IsNull::No => Ok(out.into_inner().into_value()),
    }
}

/// Rebuild one field value from the `turso::Value` at its position in the
/// record.
///
/// Routed through `from_nullable_sql` rather than `from_sql` so that a
/// stored NULL reaches `Option<T>`'s impl as `None`, and reaches a
/// non-nullable type as diesel's own "unexpected null" error rather than a
/// storage-class mismatch that says the wrong thing.
pub fn decode_field<ST, T>(value: &turso::Value) -> Result<T, Box<dyn Error + Send + Sync>>
where
    T: FromSql<ST, Turso>,
{
    match value {
        turso::Value::Null => T::from_nullable_sql(None),
        other => T::from_nullable_sql(Some(TursoValue::new(other))),
    }
}

macro_rules! field_type {
    ($rust:ty => $sql:ty) => {
        impl TursoFieldType for $rust {
            type SqlType = $sql;
        }
    };
}

field_type!(i16 => crate::sql_types::SmallInt);
field_type!(i32 => crate::sql_types::Integer);
field_type!(i64 => crate::sql_types::BigInt);
field_type!(bool => crate::sql_types::Bool);
field_type!(f32 => crate::sql_types::Float);
field_type!(f64 => crate::sql_types::Double);
field_type!(String => crate::sql_types::Text);
field_type!(Vec<u8> => crate::sql_types::Binary);
field_type!(uuid::Uuid => crate::turso::sql_types::Uuid);

#[cfg(feature = "chrono")]
mod chrono_field_types {
    use super::TursoFieldType;
    use ::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

    field_type!(NaiveDate => crate::sql_types::Date);
    field_type!(NaiveTime => crate::sql_types::Time);
    field_type!(NaiveDateTime => crate::sql_types::Timestamp);
    field_type!(DateTime<Utc> => crate::turso::sql_types::Timestamptz);
}

#[cfg(feature = "gpui")]
field_type!(gpui::SharedString => crate::sql_types::Text);

// Deliberately absent: `Vec<String>` and `Vec<gpui::SharedString>`. They do
// have Turso codecs — `crate::turso::string_list` stores a string list as a JSON
// array in a TEXT column — but that is a storage decision specific to the
// two composite fields that use it, not a fact about string lists, and a
// default here would silently apply it to every `Vec<String>` anybody ever
// puts in a variant. Those fields spell `#[union(sql_type = Text)]`.

/// An optional field is the nullable form of its inner field's SQL type,
/// which is exactly the shape diesel's `Option<T>` impls want. This is the
/// one blanket in the file, and it is why no `Option<…>` needs an impl of
/// its own.
impl<T: TursoFieldType> TursoFieldType for Option<T> {
    type SqlType = Nullable<T::SqlType>;
}
