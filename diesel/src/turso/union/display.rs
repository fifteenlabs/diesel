//! Human-readable rendering of UNION blobs, as `{ type: <variant>,
//! <field>: <value>, … }` (not JSON — no quoting).
//!
//! Delegates to [`crate::turso::union::wire`] for the wire-format decode so this
//! module stays agnostic of SQLite's record format and can't drift from
//! the writer side.

use crate::turso::union::{UnionSchema, wire};

/// Render a UNION blob against `T`'s layout, or `None` if the bytes do
/// not decode as one.
///
/// `None` rather than an error because the only callers are diagnostic —
/// a CLI column renderer and test output — and a half-rendered row is
/// more use to them than a failure.
pub fn render_blob<T: UnionSchema>(blob: &[u8]) -> Option<String> {
    let (tag_index, outer) = wire::decode_union(blob).ok()?;
    let tag = *T::variants().get(tag_index as usize)?;
    let fields = *T::variant_fields().get(tag_index as usize)?;
    let turso::Value::Blob(inner) = outer else {
        return None;
    };
    let values = wire::decode_record(&inner).ok()?;
    if values.len() != fields.len() {
        return None;
    }
    let pairs: Vec<String> = fields
        .iter()
        .zip(values.iter())
        .map(|(f, v)| format!("{f}: {}", render_value(v)))
        .collect();
    Some(format!("{{ type: {tag}, {} }}", pairs.join(", ")))
}

fn render_value(v: &turso::Value) -> String {
    match v {
        turso::Value::Null => "null".into(),
        turso::Value::Integer(n) => n.to_string(),
        turso::Value::Real(f) => f.to_string(),
        turso::Value::Text(s) => s.clone(),
        turso::Value::Blob(b) => format!("<blob {} bytes>", b.len()),
    }
}
