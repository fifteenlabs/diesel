//! UNION / STRUCT custom-type support for the Turso backend.
//!
//! # Wire format
//!
//! Turso stores a UNION-typed column as a blob:
//!
//! ```text
//! [tag_index: u8][outer_record: one-column SQLite record]
//! ```
//!
//! The tag index is positional in the `CREATE TYPE ... AS UNION(...)`
//! declaration; the first variant is 0, the next 1, and so on. See
//! [`wire`] for the full serial-type layout.
//!
//! # Codec
//!
//! Turso accepts a raw wire-format blob as a bind value for a UNION
//! column, so the ToSql path builds the blob client-side and binds it
//! directly. The FromSql path reads the blob and decodes it. No custom
//! SQL emission is needed — the pipeline stays inside diesel's scalar
//! `ToSql`/`FromSql` model.
//!
//! ## Positional coupling, and the four things that watch it
//!
//! The enum's variant declaration order must match the `CREATE TYPE`'s
//! UNION variant order, and a struct variant's field order must match its
//! `STRUCT(...)` member order. Both are positional, and a Turso record
//! carries only SQLite serial types — a storage class and nothing else —
//! so a mismatch does not error. Two swapped TEXT fields decode into each
//! other; a shifted tag hands back the wrong variant of a compatible
//! shape. Every row read or written under a mismatched layout is wrong and
//! nothing says so. This is the failure mode the rest of this module is
//! organised around:
//!
//! 1. [`UnionSchema::create_type_sql`] emits the DDL from the Rust enum, so
//!    there is a machine-generated copy of the ordering to compare against.
//! 2. A golden test per database (`fifteen-db`'s `union_ddl_golden`,
//!    `slackdb`'s) asserts each migration's `CREATE TYPE` text agrees with
//!    that emission, member by member. Compile-time-ish: a build failure
//!    with a diff.
//! 3. `diesel_async::turso::probe::verify_declared_types` asks the
//!    *database* at connection establish, through Turso's
//!    `sqlite_turso_types` vtab, and refuses to open a file whose stored
//!    declarations disagree. This is the only layer that can catch a file
//!    an older binary wrote.
//! 4. `turbo-diesel`'s `wire_differential` test pins our encoder against
//!    Turso's own `union_value` / `struct_pack`, byte for byte. Migrations
//!    write rows with those functions and the app then looks them up by
//!    binding *our* bytes against a UNION primary key, so a divergence
//!    makes every migrated row both unfindable and re-insertable.
//!
//! What none of the four gives is a *compile* error, and field order within
//! a struct is the case that hurts: reordering two `Option<SharedString>`
//! fields in a 30-field payload compiles clean and is caught only at (2) or
//! (3). Keep field order alone.
//!
//! # Reading a union from SQL
//!
//! The codec above is how a whole value crosses the wire. Asking the
//! *database* about one variant or one field is the other half, and it is
//! [`expr`]: `union_extract`, `struct_extract` and `union_tag` as typed
//! expression nodes, addressed through variant and field types the derive
//! emits beside the enum.
//!
//! ```ignore
//! use fifteen_db::schema::meta::{message_id::telegram, messages};
//!
//! messages::table
//!     .filter(messages::mid.extract(telegram::variant).is_not_null())
//!     .filter(messages::mid.extract(telegram::variant).field(telegram::chat_id).eq(chat_id))
//! ```
//!
//! That replaced ~60 `dsl::sql` fragments across `fifteen-db` and
//! `fifteen-search`. Field names became paths, bind types became checked,
//! and — the reason it is worth more than tidiness — the statements became
//! cacheable: diesel marks a `SqlLiteral` unsafe to cache and the verdict
//! covers the entire query around it, so one three-function fragment cost
//! the whole statement its cached program on every call.

pub mod codec;
pub mod ddl;
pub mod display;
pub mod expr;
pub mod field_type;
pub mod schema;
pub mod wire;

pub use codec::{TaggedUnion, decode_from_blob, encode_for_bind};
pub use ddl::{
    DeclarationDrift, TypeDecl, TypeKind, check_declarations, index_by_name,
    parse_create_types,
};
pub use expr::{
    Composite, CompositeExpressionMethods, CompositeField, CompositeShape, CompositeSqlType,
    Extract, GetField, NullableComposite, NullableOf, UnionExpressionMethods, UnionSqlType,
    UnionTag, UnionVariant,
};
pub use field_type::{TursoFieldType, ddl_type_name, decode_field, encode_field};
pub use schema::{DecodeError, EncodeResult, UnionSchema, UnionStructPayload, ValueKind};
pub use wire::{WireError, decode_record, encode_record};
