//! The one helper more than one module in this suite needs: the SQL a query
//! renders to.

use diesel::turso::Turso;

/// The SQL a query renders to, without the bind values `debug_query` appends
/// after it.
///
/// Two kinds of assertion are built on this. One is "the dialect wrote what it
/// was supposed to write", which only ever wanted the text. The other is
/// `EXPLAIN QUERY PLAN`, which compiles a statement instead of running it: the
/// `?` placeholders never need values, so a query can be planned straight from
/// its rendering with nothing bound and nothing executed.
pub(crate) fn rendered<Q>(query: &Q) -> String
where
    Q: diesel::query_builder::QueryFragment<Turso> + diesel::query_builder::QueryId,
{
    let debug = diesel::debug_query::<Turso, _>(query).to_string();
    match debug.split_once(" -- binds:") {
        Some((sql, _)) => sql.to_string(),
        None => debug,
    }
}
