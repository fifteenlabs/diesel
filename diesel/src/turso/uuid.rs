//! `uuid` integration — Turso-side codec for `uuid::Uuid` stored in a
//! `Binary` (BLOB) column.
//!
//! The backend-generic `AsExpression<Binary>` / `FromSqlRow` proxies for
//! `uuid::Uuid` are in `crate::type_impls::binary_uuid`, shared with the
//! SQLite backend, which stores a UUID the same way. All that is here is
//! the Turso-side `ToSql` / `FromSql` pair, so the round-trip produces a
//! 16-byte blob.
//!
//! A UUID inside a UNION field goes through this same pair — see
//! `union::field_type`, which is where the composite layer's own
//! conversion code used to live.

use crate::deserialize::{self, FromSql};
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types::Binary;
use uuid::Uuid;

use crate::turso::backend::Turso;
use crate::turso::value::{TursoValue, mismatch};

pub(super) mod sql_types {
    //! `Uuid` is a readability alias for `Binary` — UUIDs travel as raw
    //! 16-byte blobs. Schema files `use diesel::turso::sql_types::Uuid`
    //! and column-type `id -> Uuid` instead of the bare `Binary`.
    //!
    //! Transparency warning: because this is a type alias, `Vec<u8>` /
    //! `&[u8]` still satisfy any bound involving `sql_types::Uuid` —
    //! diesel will happily accept a raw 16-byte blob where a `uuid::Uuid`
    //! was intended. A newtype would close that hole but costs us the
    //! free `AsExpression` / `FromSqlRow` impls that come from the shared
    //! foreign proxy.
    pub type Uuid = crate::sql_types::Binary;
}

impl ToSql<Binary, Turso> for Uuid {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self.as_bytes().as_slice());
        Ok(IsNull::No)
    }
}

impl FromSql<Binary, Turso> for Uuid {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            turso::Value::Blob(b) => Uuid::from_slice(b).map_err(Into::into),
            other => mismatch("Blob", other),
        }
    }
}
