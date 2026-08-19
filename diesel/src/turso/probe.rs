//! Refusing to open a database whose stored type declarations disagree
//! with the derive's.
//!
//! # The hole this closes
//!
//! A UNION tag is a positional index and a STRUCT field is a positional
//! offset, so the Rust enum and the `CREATE TYPE` that a migration ran are
//! two copies of one ordering. A golden test pins them together *in this
//! build*. It says nothing about the file on disk, which was written by
//! whatever build ran the migration — possibly months ago, possibly from a
//! branch, possibly by a binary whose enum had one fewer variant. Reading
//! that file with today's layout does not error: a Turso record carries
//! SQLite serial types, which give a storage class and nothing else, so two
//! swapped TEXT fields decode into each other in silence and two swapped
//! tags hand back the wrong variant of the right shape.
//!
//! Turso keeps every declaration in a queryable vtab —
//! `sqlite_turso_types(name, sql)` — so the check is available and cheap:
//! one statement over a table of a few dozen rows, at connection establish,
//! before any application query. It stays on in release builds because it
//! is the *only* layer that can see an existing file. Compile-time checks
//! and golden tests both describe the binary; this describes the data.
//!
//! # What it compares
//!
//! Member-by-member, ignoring formatting and case, over every `CREATE TYPE`
//! the derive emits — the union itself and each of its struct-variant
//! payload types. Turso re-renders the statement from its own AST on the way
//! out, so string equality is not on the table; see
//! [`union::ddl`](super::union::ddl).
//!
//! Extra declarations in the database are fine and expected: superseded
//! versions (`social_id_v5` next to `social_id_v6`) can never be dropped
//! while any column still names them, and Turso's built-in domains show up
//! in the same vtab.
//!
//! # Registration
//!
//! The list of types to check is passed in, one hand-maintained array per
//! database crate, in the shape `fifteen-db` already maintains for the
//! CLI's column renderer. A type nobody registers is silently unchecked,
//! which is a real cost and the honest reason to say so out loud here
//! rather than reach for a distributed-slice crate: there are three entries
//! today, they live next to the enums they name, and a dependency whose
//! whole job is to save an array literal is not earned.

use std::collections::BTreeMap;

use crate::turso::connection::TursoConnection;
use crate::turso::union::ddl::{
    check_declarations, index_by_name, parse_create_types, DeclarationDrift, TypeDecl,
};

/// One composite type the app will bind values against, and where its
/// authoritative declaration comes from.
///
/// Built as `DeclaredType::of::<SocialId>()` — the `rust_type` is only for
/// the error message, and `ddl` is the derive's `create_type_sql`.
#[derive(Debug, Clone, Copy)]
pub struct DeclaredType {
    /// The Rust type whose derive produced `ddl`. Reported when the check
    /// fails, so the message names a type a reader can go and look at.
    pub rust_type: &'static str,
    /// The `CREATE TYPE …` text this binary's derive emits, as a thunk
    /// because building it allocates and the check usually passes.
    pub ddl: fn() -> String,
}

/// Name a type for [`verify_declared_types`].
///
/// A macro rather than a generic constructor so the Rust type's name in the
/// failure message is the one the reader can grep for, rather than
/// `std::any::type_name`'s fully-qualified rendering of a path that may not
/// be how the enum is imported.
#[macro_export]
macro_rules! declared_types {
    ($($ty:path),+ $(,)?) => {
        &[$(
            $crate::turso::probe::DeclaredType {
                rust_type: ::std::stringify!($ty),
                ddl: <$ty as ::diesel::turso::union::UnionSchema>::create_type_sql,
            }
        ),+]
    };
}

/// A database whose stored declarations do not match this binary's.
#[derive(Debug)]
pub struct TypeDriftError {
    /// Per registered Rust type that disagreed.
    pub drift: Vec<(&'static str, DeclarationDrift)>,
}

impl std::fmt::Display for TypeDriftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "this database's composite type declarations do not match this build's:"
        )?;
        for (rust_type, drift) in &self.drift {
            writeln!(f, "{rust_type}:")?;
            write!(f, "{drift}")?;
        }
        write!(
            f,
            "Every UNION tag and STRUCT field is positional, so reading these rows \
             with this build's layout would silently return wrong values rather than \
             fail. Either run the migration that redeclares the types, or rebuild the \
             database."
        )
    }
}

impl std::error::Error for TypeDriftError {}

/// Why a database could not be certified. Kept as two arms so a caller can
/// tell "this file disagrees with this binary" from "this file could not be
/// asked" — the first is a bug in the schema, the second is a broken file.
#[derive(Debug, thiserror::Error)]
pub enum TypeCheckError {
    /// The file and the binary disagree about a declaration — a schema bug.
    #[error("{0}")]
    Drift(#[from] TypeDriftError),
    /// The declarations could not be read at all — a broken file.
    #[error("reading sqlite_turso_types: {0}")]
    Read(#[from] turso::Error),
}

/// Read every composite declaration out of `sqlite_turso_types`.
///
/// Goes through the raw `turso::Connection` rather than the diesel DSL
/// because a vtab has no `table!`, and through `prepare` rather than
/// `prepare_cached` because this runs once per connection and a one-shot
/// statement is what a one-shot query should compile to.
async fn stored_declarations(
    conn: &TursoConnection,
) -> Result<BTreeMap<String, TypeDecl>, turso::Error> {
    let mut stmt = conn
        .raw()
        .prepare("SELECT sql FROM sqlite_turso_types")
        .await?;
    let mut rows = stmt.query(()).await?;
    let mut decls = Vec::new();
    while let Some(row) = rows.next().await? {
        if let turso::Value::Text(sql) = row.get_value(0)? {
            decls.extend(parse_create_types(&sql));
        }
    }
    Ok(index_by_name(decls))
}

/// Check `types` against what this database says it stores.
///
/// `waived` names composite types whose stored declaration is known to
/// differ and cannot be brought into line — a `CREATE TYPE` that shipped
/// wrong, which Turso offers no way to alter and which is not worth a table
/// rebuild. A waiver is a deliberate hole in the check and every entry
/// should carry, at its call site, why the difference is inert. Prefer an
/// empty slice.
#[tracing::instrument(skip_all, fields(types = types.len()))]
pub async fn verify_declared_types(
    conn: &TursoConnection,
    types: &[DeclaredType],
    waived: &[&str],
) -> Result<(), TypeCheckError> {
    if types.is_empty() {
        return Ok(());
    }
    let stored = stored_declarations(conn).await?;
    let drift: Vec<_> = types
        .iter()
        .filter_map(|t| {
            let mut drift = check_declarations(&(t.ddl)(), &stored);
            drift.missing.retain(|n| !waived.contains(&n.as_str()));
            drift
                .differs
                .retain(|(n, ..)| !waived.contains(&n.as_str()));
            (!drift.is_empty()).then_some((t.rust_type, drift))
        })
        .collect();
    if drift.is_empty() {
        tracing::debug!("composite type declarations verified");
        Ok(())
    } else {
        Err(TypeDriftError { drift }.into())
    }
}
