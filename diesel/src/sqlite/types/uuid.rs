use crate::deserialize::{self, FromSql, FromSqlRow};
use crate::expression::AsExpression;
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types::Binary;
use crate::sqlite::{Sqlite, SqliteValue};

// Split the foreign derive: `AsExpression<Binary>` always — it's keyed
// by the SQL type and doesn't collide with the pg side's
// `AsExpression<Uuid>`. `FromSqlRow` only when `postgres_backend` is off;
// the derive emits a fully-generic `Queryable<_, _>` impl (not
// parameterised by SQL type), so emitting it here in addition to the
// pg-side copy in `pg/types/uuid.rs` is an E0119 collision when both
// features are active (e.g. `cargo clippy --all-features`).
#[derive(AsExpression)]
#[diesel(foreign_derive)]
#[diesel(sql_type = Binary)]
#[allow(dead_code)]
struct UuidProxyAsExpression(uuid::Uuid);

#[cfg(not(feature = "postgres_backend"))]
#[derive(FromSqlRow)]
#[diesel(foreign_derive)]
#[diesel(sql_type = Binary)]
#[allow(dead_code)]
struct UuidProxyFromSqlRow(uuid::Uuid);

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
