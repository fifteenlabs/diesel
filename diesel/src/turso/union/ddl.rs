//! Reading `CREATE TYPE … AS STRUCT/UNION(…)` back out of SQL text, so a
//! declaration can be compared against the one `#[derive(UnionSchema)]`
//! emits.
//!
//! # Why a parser rather than a string compare
//!
//! Three places spell the same layout and all three are load-bearing: the
//! Rust enum, the `CREATE TYPE` in a migration, and the declaration Turso
//! kept when that migration ran (readable back from the
//! `sqlite_turso_types` vtab). None of the three agrees on formatting.
//! Migrations column-align their member lists and carry `--` comments;
//! Turso re-renders the statement from its own AST when it hands it back
//! (it is *not* the verbatim text, whatever `TypeDef.sql`'s comment says);
//! the derive emits one line with single spaces. Comparing the strings
//! would report drift on every type. Comparing member-by-member reports it
//! on exactly the ones that differ, and can say which member.
//!
//! What a comparison is allowed to ignore is therefore whitespace,
//! comments, and identifier case — Turso lowercases type-registry keys, so
//! case never survives a round trip anyway. What it must not ignore is
//! member *order*: tag indices and struct field positions are positional
//! on the wire, so two declarations that differ only in the order of two
//! same-typed members describe databases that decode each other's rows
//! into the wrong fields. That is the failure this module exists to make
//! loud, and it is why [`TypeDecl::members`] is a `Vec` and not a map.

use std::collections::BTreeMap;
use std::fmt;

/// Which flavour of composite a `CREATE TYPE` declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Struct,
    Union,
}

impl TypeKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Struct => "STRUCT",
            Self::Union => "UNION",
        }
    }
}

/// One parsed `CREATE TYPE <name> AS STRUCT|UNION(<members>)`.
///
/// `members` are `(name, type)` pairs in declaration order — for a UNION
/// the position *is* the wire tag, for a STRUCT it is the field index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDecl {
    pub name: String,
    pub kind: TypeKind,
    pub members: Vec<(String, String)>,
}

impl TypeDecl {
    /// The comparison key: everything lowercased, one space per gap. Two
    /// declarations describing the same layout produce the same string
    /// however they were written.
    pub fn canonical(&self) -> String {
        let members = self
            .members
            .iter()
            .map(|(n, t)| format!("{} {}", n.to_lowercase(), t.to_lowercase()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "CREATE TYPE {} AS {}({})",
            self.name.to_lowercase(),
            self.kind.as_str(),
            members
        )
    }
}

impl fmt::Display for TypeDecl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

/// Every `CREATE TYPE … AS STRUCT/UNION(…)` in `sql`, in the order they
/// appear.
///
/// Anything else that starts `CREATE TYPE` is skipped rather than being an
/// error, because Turso's own built-ins are domains
/// (`CREATE TYPE bigint(value integer) BASE integer`) and the vtab hands
/// them back alongside ours. A domain declares no internal layout, so
/// there is nothing here to compare.
pub fn parse_create_types(sql: &str) -> Vec<TypeDecl> {
    let sql = strip_comments(sql);
    let bytes: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut cursor_start = 0usize;

    while let Some(i) = find_word(&bytes, cursor_start, "create") {
        cursor_start = i;
        let Some(after_type) = expect_word(&bytes, i, "type") else {
            continue;
        };
        let Some((name, after_name)) = read_ident(&bytes, after_type) else {
            continue;
        };
        // A domain spells its parameter list right after the name; skip
        // it so the `AS` check below sees the real shape.
        let mut cursor = skip_ws(&bytes, after_name);
        if bytes.get(cursor) == Some(&'(') {
            let Some((_, after_params)) = read_paren_group(&bytes, cursor) else {
                continue;
            };
            cursor = after_params;
        }
        let Some(after_as) = expect_word(&bytes, cursor, "as") else {
            continue; // a domain (`BASE …`), not a composite
        };
        let Some((kind_word, after_kind)) = read_ident(&bytes, after_as) else {
            continue;
        };
        let kind = match kind_word.to_lowercase().as_str() {
            "struct" => TypeKind::Struct,
            "union" => TypeKind::Union,
            _ => continue,
        };
        let cursor = skip_ws(&bytes, after_kind);
        let Some((body, _)) = read_paren_group(&bytes, cursor) else {
            continue;
        };
        out.push(TypeDecl {
            name,
            kind,
            members: split_members(&body),
        });
    }
    out
}

/// Index the declarations by lowercased name, which is how both sides of
/// every comparison look one up.
pub fn index_by_name(decls: impl IntoIterator<Item = TypeDecl>) -> BTreeMap<String, TypeDecl> {
    decls
        .into_iter()
        .map(|d| (d.name.to_lowercase(), d))
        .collect()
}

/// What a comparison found. Empty means the two sides agree.
#[derive(Debug, Default, Clone)]
pub struct DeclarationDrift {
    /// Types the derive declares that the other side has never heard of.
    pub missing: Vec<String>,
    /// `(type name, what the derive says, what the other side says)`.
    pub differs: Vec<(String, String, String)>,
}

impl DeclarationDrift {
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.differs.is_empty()
    }
}

impl fmt::Display for DeclarationDrift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for name in &self.missing {
            writeln!(f, "  {name}: not declared")?;
        }
        for (name, derived, stored) in &self.differs {
            writeln!(f, "  {name}:")?;
            writeln!(f, "    derive: {derived}")?;
            writeln!(f, "    stored: {stored}")?;
        }
        Ok(())
    }
}

impl std::error::Error for DeclarationDrift {}

/// Check every `CREATE TYPE` a derive emits against declarations from
/// somewhere else.
///
/// One-directional on purpose. `stored` legitimately holds types the
/// derive knows nothing about — every superseded `social_id_v5`, every
/// Turso built-in domain — and flagging those would make the check
/// useless. What matters is that each type the Rust side *will bind
/// against* is declared, and declared with the layout the Rust side
/// encodes.
pub fn check_declarations(
    derived_ddl: &str,
    stored: &BTreeMap<String, TypeDecl>,
) -> DeclarationDrift {
    let mut drift = DeclarationDrift::default();
    for decl in parse_create_types(derived_ddl) {
        match stored.get(&decl.name.to_lowercase()) {
            None => drift.missing.push(decl.name.clone()),
            Some(found) if found.canonical() != decl.canonical() => {
                drift
                    .differs
                    .push((decl.name.clone(), decl.canonical(), found.canonical()));
            }
            Some(_) => {}
        }
    }
    drift
}

// -- scanning helpers --------------------------------------------------------

/// Blank out `--` line comments, leaving string literals alone. Migration
/// text carries both, and a `--` inside a quoted string is data.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_string = !in_string;
                out.push(c);
            }
            '-' if !in_string && chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The position just past the next case-insensitive occurrence of `word`
/// as a whole identifier, at or after `from`.
fn find_word(chars: &[char], from: usize, word: &str) -> Option<usize> {
    let needle: Vec<char> = word.chars().collect();
    let mut i = from;
    while i + needle.len() <= chars.len() {
        let matches = chars[i..i + needle.len()]
            .iter()
            .zip(&needle)
            .all(|(a, b)| a.eq_ignore_ascii_case(b));
        let preceded_by_ident = i > 0 && is_ident_char(chars[i - 1]);
        let followed_by_ident = chars
            .get(i + needle.len())
            .is_some_and(|c| is_ident_char(*c));
        if matches && !preceded_by_ident && !followed_by_ident {
            return Some(i + needle.len());
        }
        i += 1;
    }
    None
}

fn skip_ws(chars: &[char], mut i: usize) -> usize {
    while chars.get(i).is_some_and(|c| c.is_whitespace()) {
        i += 1;
    }
    i
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Consume `word` (case-insensitively) at `i`, returning the position
/// after it, or `None` if something else is there.
fn expect_word(chars: &[char], i: usize, word: &str) -> Option<usize> {
    let start = skip_ws(chars, i);
    let (found, after) = read_ident(chars, start)?;
    found.eq_ignore_ascii_case(word).then_some(after)
}

fn read_ident(chars: &[char], i: usize) -> Option<(String, usize)> {
    let start = skip_ws(chars, i);
    let mut end = start;
    while chars.get(end).is_some_and(|c| is_ident_char(*c)) {
        end += 1;
    }
    (end > start).then(|| (chars[start..end].iter().collect(), end))
}

/// Read a balanced `(...)` group starting at `i`, returning its inner text
/// and the position after the closing paren.
fn read_paren_group(chars: &[char], i: usize) -> Option<(String, usize)> {
    let start = skip_ws(chars, i);
    if chars.get(start) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    let mut end = start;
    while let Some(&c) = chars.get(end) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    let body: String = chars[start + 1..end].iter().collect();
                    return Some((body, end + 1));
                }
            }
            _ => {}
        }
        end += 1;
    }
    None
}

/// Split a member list on top-level commas, then each member on its first
/// whitespace run: `"  telegram_user   INT "` → `("telegram_user", "INT")`.
fn split_members(body: &str) -> Vec<(String, String)> {
    let mut members = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for c in body.chars() {
        match c {
            '(' => {
                depth += 1;
                current.push(c);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => {
                push_member(&mut members, &current);
                current.clear();
            }
            _ => current.push(c),
        }
    }
    push_member(&mut members, &current);
    members
}

fn push_member(members: &mut Vec<(String, String)>, raw: &str) {
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    match raw.split_once(char::is_whitespace) {
        Some((name, ty)) => members.push((
            name.to_string(),
            ty.split_whitespace().collect::<Vec<_>>().join(" "),
        )),
        // A member with no type at all is malformed SQL; carry it through
        // as an empty type so the comparison reports it rather than
        // silently dropping the member and shifting every later ordinal.
        None => members.push((raw.to_string(), String::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_shapes_migrations_actually_write() {
        let decls = parse_create_types(
            "-- a comment mentioning CREATE TYPE nothing\n\
             CREATE TYPE telegram_mid AS STRUCT(\n\
             \x20   user_id   INT,   -- normalised to 0 for groups\n\
             \x20   topic_id  INT\n\
             );\n\
             CREATE TYPE message_id_v4 AS UNION(telegram telegram_mid, email INT);",
        );
        assert_eq!(decls.len(), 2);
        assert_eq!(decls[0].name, "telegram_mid");
        assert_eq!(decls[0].kind, TypeKind::Struct);
        assert_eq!(
            decls[0].members,
            vec![
                ("user_id".to_string(), "INT".to_string()),
                ("topic_id".to_string(), "INT".to_string()),
            ]
        );
        assert_eq!(
            decls[1].canonical(),
            "CREATE TYPE message_id_v4 AS UNION(telegram telegram_mid, email int)"
        );
    }

    #[test]
    fn skips_domains_and_other_create_statements() {
        let decls = parse_create_types(
            "CREATE TYPE bigint(value integer) BASE integer;\
             CREATE TYPE uuid(value text) BASE blob ENCODE uuid_blob(value);\
             CREATE TABLE t(a INT);\
             CREATE TYPE x AS STRUCT(a INT);",
        );
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0].name, "x");
    }

    #[test]
    fn formatting_and_case_do_not_count_as_drift() {
        let a = parse_create_types("create type Foo as struct(A int, B text)");
        let b = parse_create_types("CREATE TYPE foo AS STRUCT(\n  a   INT,\n  b   TEXT\n)");
        assert_eq!(a[0].canonical(), b[0].canonical());
    }

    /// The whole point: two same-typed members swapped is drift, even
    /// though the set of members is identical.
    #[test]
    fn member_order_counts_as_drift() {
        let derived = "CREATE TYPE p AS STRUCT(first_name TEXT, last_name TEXT)";
        let stored = index_by_name(parse_create_types(
            "CREATE TYPE p AS STRUCT(last_name TEXT, first_name TEXT)",
        ));
        let drift = check_declarations(derived, &stored);
        assert!(drift.missing.is_empty());
        assert_eq!(drift.differs.len(), 1);
        assert_eq!(drift.differs[0].0, "p");
    }

    #[test]
    fn reports_a_type_the_database_never_declared() {
        let stored = index_by_name(parse_create_types("CREATE TYPE other AS STRUCT(a INT)"));
        let drift = check_declarations("CREATE TYPE p AS STRUCT(a INT)", &stored);
        assert_eq!(drift.missing, vec!["p".to_string()]);
    }

    /// Extra declarations on the stored side are not drift — superseded
    /// union versions live in the same database forever.
    #[test]
    fn extra_stored_types_are_ignored() {
        let stored = index_by_name(parse_create_types(
            "CREATE TYPE p AS STRUCT(a INT); CREATE TYPE p_v0 AS STRUCT(b TEXT)",
        ));
        let drift = check_declarations("CREATE TYPE p AS STRUCT(a INT)", &stored);
        assert!(drift.is_empty(), "{drift}");
    }
}
