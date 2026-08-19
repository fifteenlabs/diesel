//! Typed `PRAGMA` statements.
//!
//! A PRAGMA names no table, so there is nothing for `table!` to hang a DSL
//! off — which is why every one of them used to be a `crate::sql_query`
//! string at the call site. That is the shape of mistake the string form
//! makes easy and silent: `PRAGMA foreign_key = ON` (singular) is not an
//! error in SQLite, it is an unknown pragma, which is a no-op. The database
//! then runs for the rest of its life without foreign keys and the only
//! symptom is orphan rows nobody notices for a month.
//!
//! So the pragmas we actually run get a function each. The name is spelled
//! once, here, and the value is a Rust type rather than a fragment of SQL —
//! the call site cannot misspell either. Adding a pragma means adding a
//! function; there is deliberately no `pragma("anything you like")` door,
//! because that would be `sql_query` again with extra steps.
//!
//! PRAGMA arguments cannot be bind parameters in SQLite (`PRAGMA
//! foreign_keys = ?` is a parse error), so the value is rendered into the
//! SQL text. Every constructor here takes a typed Rust value and maps it to
//! a fixed keyword, so no caller-supplied string ever reaches the text.

use crate::query_builder::{AstPass, QueryFragment, QueryId};

use crate::turso::backend::Turso;

/// `PRAGMA foreign_keys = ON | OFF`.
///
/// Per *connection*, not per database: SQLite and Turso both default it to
/// off, and a connection that forgets it silently accepts writes that
/// violate every `REFERENCES` in the schema.
///
/// Establishing a connection already runs `ON` — see
/// [`TursoConnection::establish_for_migrations`](crate::turso::TursoConnection::establish_for_migrations)
/// for the single door that doesn't — so a caller reaching for this is
/// either turning enforcement *off* around a table rebuild, or turning it
/// back on afterwards. Both spellings exist for that pair.
pub fn foreign_keys(enabled: bool) -> ForeignKeys {
    ForeignKeys { enabled }
}

/// The statement returned by [`foreign_keys`].
#[derive(Debug, Clone, Copy)]
pub struct ForeignKeys {
    enabled: bool,
}

impl ForeignKeys {
    /// The statement as text.
    ///
    /// Exists because [`TursoConnection::open`](crate::turso::TursoConnection)
    /// runs this pragma before the connection is handed to anyone, and runs it
    /// through `batch_execute` rather than the DSL: a statement executed the
    /// normal way is offered to the statement cache, and one that runs exactly
    /// once per connection would then hold a cache slot for the life of the
    /// connection and show up in every measurement of what the cache admitted.
    /// Both spellings of the text come from here, so they cannot drift.
    pub(crate) const fn sql(self) -> &'static str {
        if self.enabled {
            "PRAGMA foreign_keys = ON"
        } else {
            "PRAGMA foreign_keys = OFF"
        }
    }
}

impl QueryFragment<Turso> for ForeignKeys {
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        out.push_sql(self.sql());
        Ok(())
    }
}

impl QueryId for ForeignKeys {
    type QueryId = ();
    // The SQL text depends on `enabled`, so there is no one static id for
    // this type. It costs nothing: this runs once per connection, and a
    // statement that runs once has no cache to miss.
    const HAS_STATIC_QUERY_ID: bool = false;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_both_states() {
        assert_eq!(
            crate::debug_query::<Turso, _>(&foreign_keys(true)).to_string(),
            "PRAGMA foreign_keys = ON -- binds: []"
        );
        assert_eq!(
            crate::debug_query::<Turso, _>(&foreign_keys(false)).to_string(),
            "PRAGMA foreign_keys = OFF -- binds: []"
        );
    }
}
