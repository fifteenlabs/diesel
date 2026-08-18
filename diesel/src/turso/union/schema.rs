//! The `UnionSchema` trait — implemented by user enums to describe a
//! Turso UNION type. `#[derive(UnionSchema)]` produces impls mechanically;
//! hand-written impls work equally well.
//!
//! # Scalar vs. struct variants
//!
//! Turso's UNION wire format carries exactly one value after the tag
//! byte (framed as SQLite's single-column record). For **scalar**
//! variants (`UNION(i INT, f REAL)`) that value IS the scalar; for
//! **struct** variants (`UNION(telegram STRUCT(...))`) that value is a
//! BLOB containing the inner struct's own SQLite record.
//!
//! The `encode_outer` / `decode` contract unifies both cases: each
//! variant emits and consumes a single `turso::Value` — scalar for
//! scalar variants, `Value::Blob` for struct variants.

use thiserror::Error;

/// What an encode hands back on failure — diesel's own boxed serialize
/// error, since every encode in this module is ultimately a `ToSql` call.
pub type EncodeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Storage-class tag for `TypeMismatch` errors. Kept separate from
/// `turso::Value` so errors don't carry arbitrary payloads (which would
/// break `Eq` / `Clone` / `'static` for callers that need them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl ValueKind {
    pub fn of(v: &turso::Value) -> Self {
        match v {
            turso::Value::Null => Self::Null,
            turso::Value::Integer(_) => Self::Integer,
            turso::Value::Real(_) => Self::Real,
            turso::Value::Text(_) => Self::Text,
            turso::Value::Blob(_) => Self::Blob,
        }
    }
}

impl std::fmt::Display for ValueKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Null => "NULL",
            Self::Integer => "INTEGER",
            Self::Real => "REAL",
            Self::Text => "TEXT",
            Self::Blob => "BLOB",
        })
    }
}

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error(transparent)]
    Wire(#[from] super::wire::WireError),

    /// The wire tag index didn't name a variant of the target enum.
    #[error("unknown union variant: tag index {index} (expected one of {expected:?})")]
    UnknownVariant {
        index: u8,
        expected: &'static [&'static str],
    },

    /// A struct variant's inner record had the wrong number of fields.
    #[error("union variant {variant:?}: expected {expected} fields, got {got}")]
    FieldCount {
        variant: &'static str,
        expected: usize,
        got: usize,
    },

    /// A field's `turso::Value` storage class didn't match the Rust type
    /// we were trying to decode into.
    #[error("expected {expected}, got {got_kind}")]
    TypeMismatch {
        expected: &'static str,
        got_kind: ValueKind,
    },

    /// A field's stored value didn't decode into the Rust type the layout
    /// says lives at that position.
    ///
    /// Names the field rather than only the storage classes, because the
    /// thing this most often means is that the declared field order and
    /// the enum's field order have drifted apart, and the first useful
    /// question is *which* field. (When the two drifted fields share a
    /// storage class it decodes silently instead — see the module docs on
    /// what watches for that.)
    #[error("field {variant}.{field}: {message}")]
    Field {
        variant: &'static str,
        field: &'static str,
        message: String,
    },

    /// A narrowing integer cast failed (e.g. i64 → i16 overflow).
    #[error("integer {value} out of range for {target}")]
    IntOutOfRange { value: i64, target: &'static str },

    /// A TEXT payload was not valid UTF-8.
    #[error("TEXT was not valid UTF-8: {0}")]
    Utf8(#[from] std::string::FromUtf8Error),

    /// Escape hatch for hand-written impls whose failure modes don't
    /// fit the typed variants above. Prefer a typed variant when one
    /// applies — `Custom` loses structure on the way out.
    #[error("{0}")]
    Custom(String),
}

/// Describes a Turso UNION type implemented as a Rust enum.
///
/// Variants are identified by a `u8` tag index matching their
/// declaration order in the `CREATE TYPE ... AS UNION(...)` DDL. The
/// index is the primary identity — string tags are a convenience for
/// DDL emission and error messages.
pub trait UnionSchema: Sized {
    /// Name used in `CREATE TYPE <name> AS UNION(...)`.
    fn type_name() -> &'static str;

    /// Variant tag names in declaration order. Index into this slice
    /// equals the tag byte sent over the wire.
    fn variants() -> &'static [&'static str];

    /// Field names per variant, aligned with `variants()`. Struct variants
    /// emit their declaration-order field idents; scalar variants emit an
    /// empty slice. Source of truth for rendering (e.g. `fifteen-cli db`)
    /// so the field layout never drifts from the Rust enum.
    fn variant_fields() -> &'static [&'static [&'static str]];

    /// Tag byte for this instance.
    fn tag_index(&self) -> u8;

    /// Encode this instance to the single `turso::Value` that lives as
    /// the outer record's lone column on the wire.
    ///
    /// * Scalar variants return the scalar value directly.
    /// * Struct variants return `Value::Blob(sqlite_record(fields))`.
    ///
    /// Fallible because field conversion is `ToSql`, which is fallible —
    /// none of the impls in this crate fail today, but a user newtype's
    /// may, and swallowing that would write a wrong row rather than
    /// refusing to write one.
    fn encode_outer(&self) -> EncodeResult<turso::Value>;

    /// Rebuild a variant from its tag byte and the outer-record value.
    fn decode(index: u8, outer: turso::Value) -> Result<Self, DecodeError>;

    /// Full `CREATE TYPE ...` DDL: one `AS STRUCT(...)` statement per
    /// distinct struct-variant payload type, then the `AS UNION(...)`.
    /// Statements are `; `-separated, so the whole string goes to
    /// `batch_execute` as one script.
    ///
    /// This is the machine-generated copy of the layout that the golden
    /// tests compare migrations against and that
    /// [`verify_declared_types`](crate::turso::union::verify_declared_types)
    /// compares an open database against.
    fn create_type_sql() -> String;

    /// Human-readable tag name for this instance. Default derives from
    /// `tag_index` + `variants`; override only if you need a different
    /// mapping for display (the wire tag is still the index).
    fn tag(&self) -> &'static str {
        let idx = self.tag_index() as usize;
        Self::variants()
            .get(idx)
            .copied()
            .unwrap_or("<out-of-range>")
    }

    /// Render a column blob as `{ type: <variant>, <field>: <value>, … }`.
    /// Returns `None` on malformed input or unknown variants.
    fn display_from_blob(blob: &[u8]) -> Option<String> {
        crate::turso::union::display::render_blob::<Self>(blob)
    }
}

/// A standalone struct whose fields lay out as a UNION struct-variant's body.
///
/// Lets `#[union(boxed)]` enum variants delegate their wire format to a
/// separate payload type — the enum holds `Box<Payload>`, keeping the variant
/// one pointer wide on the stack instead of inlining the full field set.
/// Ergonomics stay close to struct variants: `SocialData::SignalContact(p)`
/// with `p.field` access.
pub trait UnionStructPayload: Sized {
    /// Declaration-order field names. Associated const (not a fn) so the
    /// `UnionSchema` derive can splice it into the enum's `variant_fields`
    /// static slice under const-promotion.
    const FIELD_NAMES: &'static [&'static str];

    /// `(name, sql_type)` pairs aligned with [`FIELD_NAMES`][Self::FIELD_NAMES],
    /// used to emit the variant's `CREATE TYPE … AS STRUCT(...)`. The type
    /// strings come from
    /// [`ddl_type_name`](crate::turso::union::ddl_type_name), so they are the
    /// storage classes the bind path actually produces.
    fn sql_struct_fields() -> Vec<(&'static str, &'static str)>;

    /// Encode the payload's fields in declaration order.
    fn encode_fields(&self) -> EncodeResult<Vec<turso::Value>>;

    /// Rebuild the payload from field values in declaration order.
    fn decode_fields(values: Vec<turso::Value>) -> Result<Self, DecodeError>;
}
