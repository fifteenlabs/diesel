mod date_and_time;
mod decimal;
#[cfg(all(
    feature = "serde_json",
    any(
        feature = "postgres_backend",
        feature = "mysql_backend",
        feature = "sqlite"
    )
))]
mod json;
mod option;
// SQLite and Turso both store a `uuid::Uuid` as a raw `Binary` blob and
// need the same foreign derives to do it; one copy, shared.
#[cfg(all(
    feature = "uuid",
    any(feature = "sqlite", feature = "turso")
))]
mod binary_uuid;
mod primitives;
pub(crate) mod tuples;
