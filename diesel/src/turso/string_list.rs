//! String lists as a JSON array in a TEXT column.
//!
//! A list of strings has no storage class of its own in a STRICT database,
//! so somebody has to pick an encoding. JSON is the pick: unambiguous for
//! arbitrary strings (a separator character never has to be forbidden),
//! readable in `fifteen db`, and cheap enough to filter with
//! `LIKE '%"tag"%'` when an exact-membership scan will do.
//!
//! Scoped as narrowly as a decision this arbitrary deserves. There is no
//! `TursoFieldType` default pointing a `Vec<String>` at `Text`, so no
//! composite field picks this up by accident — the two fields that want it
//! say `#[union(sql_type = Text)]` and mean it. Nor is there a
//! `sql_types::JsonText<T>`: inventing a SQL type for two call sites buys
//! nothing that a `ToSql<Text, Turso>` impl does not already buy.
//!
//! Read is lenient about one thing only: it is JSON or it is an error. A
//! non-array or a non-string element fails loudly rather than decoding to
//! an empty list, because a field that silently reads as "no labels" is a
//! thread that silently leaves the inbox.

use crate::deserialize::{self, FromSql};
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types::Text;

use crate::turso::backend::Turso;
use crate::turso::value::{mismatch, TursoValue};

impl ToSql<Text, Turso> for Vec<String> {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(encode(self.iter().map(String::as_str))?);
        Ok(IsNull::No)
    }
}

impl FromSql<Text, Turso> for Vec<String> {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        match v.as_turso() {
            turso::Value::Text(s) => decode(s),
            other => mismatch("Text (JSON string array)", other),
        }
    }
}

/// Serialise a string list. Shared with the `gpui::SharedString` flavour so
/// the two cannot drift into different JSON.
pub(crate) fn encode<'a>(
    items: impl Iterator<Item = &'a str>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let items: Vec<&str> = items.collect();
    serde_json::to_string(&items).map_err(Into::into)
}

pub(crate) fn decode(s: &str) -> deserialize::Result<Vec<String>> {
    serde_json::from_str(s).map_err(|e| format!("string list from JSON {s:?}: {e}").into())
}

/// The `gpui::SharedString` flavour, for the app's composites — same JSON,
/// so a field can move between the two without a migration.
///
/// `SharedString` itself needs nothing here: the diesel fork already carries
/// a backend-generic `ToSql<Text, DB>` / `FromSql<ST, DB>` pair for it, so a
/// plain `SharedString` field has been a `Text` value all along, and the
/// `FieldCodec` impl that used to sit beside it was pure duplication. Only
/// the list needed an encoding chosen for it.
#[cfg(feature = "gpui")]
mod shared {
    use super::{decode, encode};
    use crate::deserialize::{self, FromSql};
    use crate::serialize::{self, IsNull, Output, ToSql};
    use crate::sql_types::Text;
    use gpui::SharedString;

    use crate::turso::backend::Turso;
    use crate::turso::value::{mismatch, TursoValue};

    impl ToSql<Text, Turso> for Vec<SharedString> {
        fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
            out.set_value(encode(self.iter().map(AsRef::as_ref))?);
            Ok(IsNull::No)
        }
    }

    impl FromSql<Text, Turso> for Vec<SharedString> {
        fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
            match v.as_turso() {
                turso::Value::Text(s) => {
                    Ok(decode(s)?.into_iter().map(SharedString::from).collect())
                }
                other => mismatch("Text (JSON string array)", other),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(items: Vec<String>) {
        let json = encode(items.iter().map(String::as_str)).unwrap();
        assert_eq!(decode(&json).unwrap(), items);
    }

    #[test]
    fn survives_the_characters_a_separator_would_have_forbidden() {
        roundtrip(vec![]);
        roundtrip(vec![String::new()]);
        roundtrip(vec!["INBOX".into(), "UNREAD".into()]);
        roundtrip(vec![
            "a,b".into(),
            "c\"d".into(),
            "e\\f".into(),
            "g\nh".into(),
        ]);
        roundtrip(vec!["🙂".into(), "Ünïcödé".into()]);
    }

    #[test]
    fn a_malformed_field_is_an_error_not_an_empty_list() {
        assert!(decode("not json").is_err());
        assert!(decode("{\"a\":1}").is_err());
        assert!(decode("[1,2]").is_err());
    }
}
