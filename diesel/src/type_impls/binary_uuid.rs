//! The backend-generic half of storing a `uuid::Uuid` in a `Binary` column.
//!
//! Two backends store a UUID as a bare 16-byte blob rather than as a
//! dedicated SQL type — SQLite and Turso — and both need the same two
//! foreign derives on `uuid::Uuid` to make one bindable and readable. The
//! derives cannot live in either backend's own module: they are keyed by
//! the `Binary` SQL type, not by a backend, so a copy in each would be an
//! E0119 collision the moment both features are on.
//!
//! `FromSqlRow` is the sharper edge of the same problem. That derive emits
//! a `Queryable<_, _>` impl that is generic over *both* the SQL type and
//! the backend, so it collides with the copy `pg/types/uuid.rs` emits for
//! PostgreSQL's native `Uuid` type — hence the `not(postgres_backend)`
//! gate, which is what `fix/uuid-proxy-double-derive` established.
//!
//! Being here rather than out of tree is the whole point of the fold for
//! this file: an external backend cannot write `impl AsExpression<Binary>
//! for uuid::Uuid` — both the trait and the type are foreign to it — so it
//! has to enable `diesel/sqlite` and borrow SQLite's copy, dragging in
//! `libsqlite3-sys` and a whole second backend for two derives it never
//! calls.

#[cfg(not(feature = "postgres_backend"))]
use crate::deserialize::FromSqlRow;
use crate::expression::AsExpression;
use crate::sql_types::Binary;

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
