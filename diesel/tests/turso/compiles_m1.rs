//! M1 acceptance: referencing `Turso` and `TursoConnection` in the shapes
//! a downstream crate would use must compile. Runtime behaviour (every
//! method panics) is not tested here — that arrives with M3.

use diesel::prelude::*;
use diesel::turso::Turso;
use diesel::turso::TursoConnection;

// A `table!` decl against our backend compiles. The macro's column-type
// validation exercises `HasSqlType<...>` for every referenced type plus
// the full `Backend` supertrait chain.
diesel::table! {
    users (id) {
        id -> BigInt,
        name -> Text,
        email -> Nullable<Text>,
        is_active -> Bool,
        score -> Double,
        created_at -> Timestamp,
        birthday -> Nullable<Date>,
        avatar -> Nullable<Binary>,
    }
}

// A derive-based row struct referencing the same columns compiles,
// binding the generic Backend param to `Turso`.
#[derive(Queryable)]
#[allow(dead_code)]
struct User {
    id: i64,
    name: String,
    email: Option<String>,
    is_active: bool,
    score: f64,
    created_at: String,
    birthday: Option<String>,
    avatar: Option<Vec<u8>>,
}

// A plain function signature that takes `&mut TursoConnection`. If the
// connection type isn't a valid `diesel::connection::AsyncConnection<Backend = Turso>`
// this won't compile.
#[allow(dead_code)]
async fn takes_conn(_conn: &mut TursoConnection) {
    let _backend: Turso = Turso;
}

#[test]
fn m1_types_exist() {
    // The existence of this test (and the file compiling) is the acceptance.
    let _: Turso = Turso;
}
