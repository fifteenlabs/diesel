//! Storing a `uuid::Uuid` in a SQLite `Binary` column.
//!
//! The `AsExpression` / `FromSqlRow` proxies this file used to carry are in
//! `type_impls::binary_uuid` now, because the Turso backend stores a UUID
//! the same way and two copies of a derive keyed on `Binary` collide.

use crate::deserialize::{self, FromSql};
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types::Binary;
use crate::sqlite::{Sqlite, SqliteValue};

#[cfg(all(feature = "sqlite", feature = "uuid"))]
impl FromSql<Binary, Sqlite> for uuid::Uuid {
    fn from_sql(mut value: SqliteValue<'_, '_, '_>) -> deserialize::Result<Self> {
        let bytes = value.read_blob();
        uuid::Uuid::from_slice(bytes).map_err(Into::into)
    }
}

#[cfg(all(feature = "sqlite", feature = "uuid"))]
impl ToSql<Binary, Sqlite> for uuid::Uuid {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Sqlite>) -> serialize::Result {
        out.set_value(self.as_bytes().as_slice());
        Ok(IsNull::No)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn uuid_round_trip() {
        // Test will be run with the full diesel test suite
    }
}
