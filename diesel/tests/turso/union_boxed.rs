//! The corners of the UNION support that nothing else in this suite
//! touches: `#[union(boxed)]`, `#[derive(UnionStructPayload)]`, nullable
//! UNION columns, the `ValidGrouping` impls on the expression nodes, tag
//! ordinals read by an enum that disagrees about them, and an identifier
//! carrying the one character the quoting rule exists for.
//!
//! Each section names the failure it is watching for; the short version:
//!
//! * **Boxing is a memory decision and must stay one.** `#[union(boxed)]`
//!   moves a fat variant's fields behind a pointer so the enum stops
//!   costing every other variant the fat one's width. If boxing also moved
//!   a byte on the wire, the same logical row written by a boxed build and
//!   an unboxed build would be two different byte strings — and since these
//!   values are primary keys, that is a lookup that misses and an insert
//!   that succeeds, i.e. a silently duplicated identity. So the tests below
//!   pin both halves: the size actually shrinks, and the bytes actually
//!   don't move.
//! * **A payload type is a second place the field order lives.** With
//!   `#[derive(UnionStructPayload)]` the `CREATE TYPE … AS STRUCT(…)`
//!   member list comes from the payload struct rather than from the enum
//!   variant, so the enum's derive can no longer see it. The DDL and the
//!   codec agreeing is therefore a property of two derives, not one.
//! * **Nullable union columns are a different NULL from a tag mismatch.**
//!   Both reach the DSL as `None` out of `union_extract`, and only one of
//!   them means "this row is not that variant".
//! * **Tag ordinals are positional and unchecked at compile time.** Two
//!   enums that disagree about variant order decode each other's rows into
//!   the wrong variant without erroring whenever the payload types happen
//!   to line up. That is the failure the golden DDL tests and the
//!   establish-time declaration probe exist to catch, and it is worth
//!   having one test that shows it happening.

use anyhow::Result;
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel::turso::union::{
    CompositeExpressionMethods, TaggedUnion, UnionExpressionMethods, UnionSchema,
    UnionStructPayload,
};
use diesel::turso::TursoConnection;
use diesel::{UnionSchema as DeriveUnionSchema, UnionStructPayload as DeriveUnionStructPayload};

// ── the schema under test ────────────────────────────────────────────────

/// The payload of the boxed variant, and the whole subject of the
/// `#[derive(UnionStructPayload)]` section below.
///
/// Deliberately fat — eight fields, six of them `String` — because the
/// attribute this file is about exists to keep a variant like this from
/// setting the size of the enum it lives in. A `String` is 24 bytes, so
/// inlining these costs ~176 bytes in *every* value of the enum, including
/// the `Legacy(i64)` ones that carry 8 bytes of their own.
#[derive(Debug, PartialEq, Clone, DeriveUnionStructPayload)]
pub struct FatPayload {
    pub account: String,
    pub thread: String,
    pub subject: String,
    pub snippet: String,
    pub sender: String,
    pub recipient: String,
    pub received_at: i64,
    pub labels: Option<String>,
}

/// The boxed form: the enum holds one pointer for the fat variant.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<BoxedKey>)]
#[union(name = "boxed_key")]
pub enum BoxedKey {
    #[union(boxed, struct_type = "boxed_fat_t")]
    Fat(Box<FatPayload>),
    Legacy(i64),
}

/// The unboxed twin: same variants, same tags, same field names, same
/// field order, same field types — spelled inline instead of behind a
/// `Box`. Everything below that compares the two is asking whether the
/// only difference between them really is where the bytes live in RAM.
///
/// The DDL type names have to differ (two `CREATE TYPE`s of one name is an
/// error), which is why they are spelled out rather than defaulted; the
/// *members* of those types are what the tests compare.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<InlineKey>)]
#[union(name = "inline_key")]
pub enum InlineKey {
    #[union(struct_type = "inline_fat_t")]
    Fat {
        account: String,
        thread: String,
        subject: String,
        snippet: String,
        sender: String,
        recipient: String,
        received_at: i64,
        labels: Option<String>,
    },
    Legacy(i64),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::BoxedKey;

    boxed_rows(id) {
        id -> BigInt,
        k -> TaggedUnion<BoxedKey>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::InlineKey;

    inline_rows(id) {
        id -> BigInt,
        k -> TaggedUnion<InlineKey>,
    }
}

/// A third table over the *boxed* union type, filled by Turso's own
/// `union_value` / `struct_pack` rather than by us. This is the
/// differential half: it is what the database would have written for the
/// same logical value if a migration had written it.
const NATIVE_TABLE: &str =
    "CREATE TABLE native_rows(id INTEGER PRIMARY KEY, k boxed_key NOT NULL) STRICT";

fn sample_payload() -> FatPayload {
    FatPayload {
        account: "me@example.com".into(),
        thread: "thread-1".into(),
        subject: "Re: the wire format".into(),
        snippet: "a snippet".into(),
        sender: "them@example.com".into(),
        recipient: "me@example.com".into(),
        // 0 is the payload-free SQLite serial, and the one real data hits
        // most often; keeping it here means the boxed path is compared on
        // the encoding that has the least in it.
        received_at: 0,
        labels: None,
    }
}

fn sample_inline() -> InlineKey {
    let p = sample_payload();
    InlineKey::Fat {
        account: p.account,
        thread: p.thread,
        subject: p.subject,
        snippet: p.snippet,
        sender: p.sender,
        recipient: p.recipient,
        received_at: p.received_at,
        labels: p.labels,
    }
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&BoxedKey::create_type_sql()).await?;
    conn.batch_execute(&InlineKey::create_type_sql()).await?;
    conn.batch_execute(
        "CREATE TABLE boxed_rows(id INTEGER PRIMARY KEY, k boxed_key NOT NULL) STRICT",
    )
    .await?;
    conn.batch_execute(
        "CREATE TABLE inline_rows(id INTEGER PRIMARY KEY, k inline_key NOT NULL) STRICT",
    )
    .await?;
    conn.batch_execute(NATIVE_TABLE).await?;
    Ok(conn)
}

/// The raw blob a UNION column holds, which is the only place the two
/// encodings can be compared without one of them getting a chance to
/// normalise the other.
async fn stored_bytes(conn: &TursoConnection, table: &str, id: i64) -> Result<Vec<u8>> {
    let sql = format!("SELECT k FROM {table} WHERE id = ?");
    let mut stmt = conn.raw().prepare(&sql).await?;
    let mut rows = stmt.query(vec![turso::Value::Integer(id)]).await?;
    let row = rows.next().await?.expect("row was inserted");
    match row.get_value(0)? {
        turso::Value::Blob(b) => Ok(b),
        other => anyhow::bail!("union column came back as {other:?}, not a blob"),
    }
}

// ── 1. `#[union(boxed)]` ─────────────────────────────────────────────────

/// The property the attribute exists for, stated as arithmetic.
///
/// An enum is as wide as its widest variant. `FatPayload` inlined is six
/// `String`s (24 bytes each), an `i64` and an `Option<String>` — so an
/// unboxed `Fat` makes *every* `InlineKey`, including the `Legacy(i64)`
/// ones, cost that much. Boxed, the variant is one pointer, so the enum
/// collapses to pointer-plus-discriminant.
///
/// The failure this prevents is a quiet one: somebody adds a field to the
/// payload, or converts a boxed variant to an inline one during a refactor,
/// and nothing breaks — the program just starts moving an order of
/// magnitude more bytes per value through every `Vec`, every channel and
/// every match. The consuming app carries a test whose only job is to
/// notice a forgotten `#[union(boxed)]`; this is the same check at the
/// level of the derive that provides the attribute.
#[test]
fn boxing_shrinks_the_enum() {
    use std::mem::size_of;

    // The payload itself is unchanged — boxing does not make it smaller,
    // it moves it.
    assert!(
        size_of::<FatPayload>() >= 6 * size_of::<String>(),
        "the payload is supposed to be fat; it is {} bytes",
        size_of::<FatPayload>()
    );

    // A boxed variant is a pointer, so the enum is a pointer and a
    // discriminant — and `Box` is non-null, so the discriminant rides in
    // the pointer's niche and the whole enum is two words.
    assert_eq!(
        size_of::<BoxedKey>(),
        2 * size_of::<usize>(),
        "a boxed variant should leave the enum at pointer + i64 payload"
    );

    // And the unboxed twin is as wide as the payload it inlines.
    assert!(
        size_of::<InlineKey>() >= size_of::<FatPayload>(),
        "unboxed: {} bytes, payload {} bytes",
        size_of::<InlineKey>(),
        size_of::<FatPayload>()
    );
    assert!(
        size_of::<InlineKey>() > 4 * size_of::<BoxedKey>(),
        "boxing is supposed to be a large win, not a rounding one: \
         {} unboxed vs {} boxed",
        size_of::<InlineKey>(),
        size_of::<BoxedKey>()
    );
}

/// A boxed variant round-trips through a real database exactly as the
/// unboxed spelling of the same variant does.
///
/// Insert-and-read-back on its own would pass even if the boxed encoder
/// and decoder were *both* wrong in the same way, so the two enums are
/// written into the same database in the same test and the values are
/// compared field by field on the way out. The scalar variant rides along
/// because a boxed variant sits next to scalar ones in the tag ordering and
/// a boxed decode arm that consumed the wrong number of bytes would show up
/// as its neighbour breaking.
#[tokio::test(flavor = "current_thread")]
async fn a_boxed_variant_round_trips_like_the_unboxed_one() -> Result<()> {
    let mut conn = setup().await?;

    let boxed = BoxedKey::Fat(Box::new(sample_payload()));
    let inline = sample_inline();

    diesel::insert_into(boxed_rows::table)
        .values(vec![
            (boxed_rows::id.eq(1i64), boxed_rows::k.eq(boxed.clone())),
            (
                boxed_rows::id.eq(2i64),
                boxed_rows::k.eq(BoxedKey::Legacy(-7)),
            ),
        ])
        .execute(&mut conn)
        .await?;
    diesel::insert_into(inline_rows::table)
        .values(vec![
            (inline_rows::id.eq(1i64), inline_rows::k.eq(inline.clone())),
            (
                inline_rows::id.eq(2i64),
                inline_rows::k.eq(InlineKey::Legacy(-7)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let got_boxed: Vec<BoxedKey> = boxed_rows::table
        .order(boxed_rows::id.asc())
        .select(boxed_rows::k)
        .load(&mut conn)
        .await?;
    let got_inline: Vec<InlineKey> = inline_rows::table
        .order(inline_rows::id.asc())
        .select(inline_rows::k)
        .load(&mut conn)
        .await?;

    assert_eq!(
        got_boxed,
        vec![
            BoxedKey::Fat(Box::new(sample_payload())),
            BoxedKey::Legacy(-7)
        ]
    );
    assert_eq!(got_inline, vec![sample_inline(), InlineKey::Legacy(-7)]);

    // Field by field, so a swap between two same-typed `String` fields
    // (the drift the module docs single out as the one nothing catches at
    // compile time) fails here rather than comparing equal to itself.
    let BoxedKey::Fat(p) = &got_boxed[0] else {
        anyhow::bail!("row 1 decoded as the wrong variant");
    };
    let InlineKey::Fat {
        account,
        thread,
        subject,
        snippet,
        sender,
        recipient,
        received_at,
        labels,
    } = &got_inline[0]
    else {
        anyhow::bail!("row 1 decoded as the wrong variant");
    };
    assert_eq!((&p.account, &p.thread), (account, thread));
    assert_eq!((&p.subject, &p.snippet), (subject, snippet));
    assert_eq!((&p.sender, &p.recipient), (sender, recipient));
    assert_eq!((p.received_at, &p.labels), (*received_at, labels));

    Ok(())
}

/// The bytes do not move.
///
/// Three encodings of one logical value are compared: the boxed derive's,
/// the unboxed derive's, and Turso's own `union_value('fat',
/// struct_pack(…))`. All three have to be the same blob. The third is the
/// one that matters most in practice — migrations write rows with
/// `union_value`/`struct_pack` and the app then finds them by binding its
/// own encoding against a UNION primary key, so a divergence makes every
/// migrated row both unfindable and re-insertable — and the first two are
/// what says boxing had nothing to do with it.
///
/// Turso is also asked to read our blob back through `union_extract` /
/// `struct_extract`, which is the other direction: bytes we wrote have to
/// be bytes the server can take apart, not merely bytes that compare equal.
#[tokio::test(flavor = "current_thread")]
async fn boxing_does_not_change_the_wire_bytes() -> Result<()> {
    let mut conn = setup().await?;
    let p = sample_payload();

    diesel::insert_into(boxed_rows::table)
        .values((
            boxed_rows::id.eq(1i64),
            boxed_rows::k.eq(BoxedKey::Fat(Box::new(p.clone()))),
        ))
        .execute(&mut conn)
        .await?;
    diesel::insert_into(inline_rows::table)
        .values((inline_rows::id.eq(1i64), inline_rows::k.eq(sample_inline())))
        .execute(&mut conn)
        .await?;

    // The same value, built by the server out of its own primitives.
    let mut stmt = conn
        .raw()
        .prepare(
            "INSERT INTO native_rows VALUES (1, \
             union_value('fat', struct_pack(?, ?, ?, ?, ?, ?, ?, ?)))",
        )
        .await?;
    // UFCS: `RunQueryDsl` is in scope and its blanket `execute(self, conn)`
    // matches at the by-value receiver step, so it would shadow the
    // inherent `Statement::execute`.
    turso::Statement::execute(
        &mut stmt,
        vec![
            turso::Value::Text(p.account.clone()),
            turso::Value::Text(p.thread.clone()),
            turso::Value::Text(p.subject.clone()),
            turso::Value::Text(p.snippet.clone()),
            turso::Value::Text(p.sender.clone()),
            turso::Value::Text(p.recipient.clone()),
            turso::Value::Integer(p.received_at),
            turso::Value::Null,
        ],
    )
    .await?;

    let boxed_bytes = stored_bytes(&conn, "boxed_rows", 1).await?;
    let inline_bytes = stored_bytes(&conn, "inline_rows", 1).await?;
    let native_bytes = stored_bytes(&conn, "native_rows", 1).await?;

    assert_eq!(
        boxed_bytes, inline_bytes,
        "boxing changed the stored representation; it is a Rust-side \
         memory decision and must not be visible on the wire"
    );
    assert_eq!(
        boxed_bytes, native_bytes,
        "our boxed encoding disagrees with union_value/struct_pack"
    );
    // The tag byte leads the blob and is the declaration ordinal: `Fat` is
    // variant 0 of `BoxedKey`.
    assert_eq!(boxed_bytes[0], 0);

    // And the server can take our blob apart again — asked of the row we
    // wrote, not of the one it wrote itself.
    let mut rows = conn
        .raw()
        .query(
            "SELECT CAST(union_tag(k) AS TEXT), \
                    CAST(struct_extract(union_extract(k, 'fat'), 'subject') AS TEXT), \
                    struct_extract(union_extract(k, 'fat'), 'received_at'), \
                    struct_extract(union_extract(k, 'fat'), 'labels') \
             FROM boxed_rows WHERE id = 1",
            (),
        )
        .await?;
    let row = rows.next().await?.expect("row 1");
    assert_eq!(row.get_value(0)?, turso::Value::Text("fat".into()));
    assert_eq!(
        row.get_value(1)?,
        turso::Value::Text("Re: the wire format".into())
    );
    assert_eq!(row.get_value(2)?, turso::Value::Integer(0));
    assert_eq!(row.get_value(3)?, turso::Value::Null);

    Ok(())
}

/// The identifier module still resolves for a boxed variant, and the
/// expressions it builds return the right answers.
///
/// This is the ergonomic half of `#[union(boxed)]` and the part most likely
/// to rot unnoticed: a boxed variant's fields live on the *payload*'s
/// generated module (`fat_payload::subject`), not on the variant's
/// (`boxed_key::fat::…`), because the enum's derive can only see the
/// payload as a name and cannot enumerate its fields. So the two derives
/// have to meet in the middle — the payload type doubles as the composite
/// shape marker — and if they stop meeting, `.field(…)` stops compiling.
/// A test that only round-trips values would never notice.
#[tokio::test(flavor = "current_thread")]
async fn a_boxed_variants_fields_are_addressable_from_the_dsl() -> Result<()> {
    use boxed_key::fat;

    let mut conn = setup().await?;
    diesel::insert_into(boxed_rows::table)
        .values(vec![
            (
                boxed_rows::id.eq(1i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(sample_payload()))),
            ),
            (
                boxed_rows::id.eq(2i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(FatPayload {
                    subject: "second".into(),
                    received_at: 99,
                    labels: Some("INBOX".into()),
                    ..sample_payload()
                }))),
            ),
            (
                boxed_rows::id.eq(3i64),
                boxed_rows::k.eq(BoxedKey::Legacy(5)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    // Projection: one field of the boxed payload, addressed by path.
    let subjects: Vec<Option<String>> = boxed_rows::table
        .order(boxed_rows::id.asc())
        .select(
            boxed_rows::k
                .extract(fat::variant)
                .field(fat_payload::subject),
        )
        .load(&mut conn)
        .await?;
    assert_eq!(
        subjects,
        vec![
            Some("Re: the wire format".into()),
            Some("second".into()),
            // The `Legacy` row is not a `fat`, so `union_extract` is NULL
            // and so is everything projected out of it.
            None,
        ]
    );

    // Comparison: the bind is typed as the payload declares the field, so
    // `received_at` compares against an i64 and `labels` against a &str.
    let ids: Vec<i64> = boxed_rows::table
        .filter(
            boxed_rows::k
                .extract(fat::variant)
                .field(fat_payload::received_at)
                .eq(99i64),
        )
        .select(boxed_rows::id)
        .load(&mut conn)
        .await?;
    assert_eq!(ids, vec![2]);

    let ids: Vec<i64> = boxed_rows::table
        .filter(
            boxed_rows::k
                .extract(fat::variant)
                .field(fat_payload::labels)
                .eq("INBOX"),
        )
        .select(boxed_rows::id)
        .load(&mut conn)
        .await?;
    assert_eq!(ids, vec![2]);

    // And the variant marker itself carries the tag the DDL spells.
    assert_eq!(fat::TAG_NAME, "fat");
    let tags: Vec<String> = boxed_rows::table
        .filter(boxed_rows::k.union_tag().eq(fat::TAG_NAME))
        .select(boxed_rows::k.union_tag())
        .order(boxed_rows::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(tags, vec!["fat", "fat"]);

    Ok(())
}

// ── 2. `#[derive(UnionStructPayload)]` ───────────────────────────────────

/// What the payload derive claims about itself, against what the enum's
/// derive then writes into the DDL.
///
/// The member list of a boxed variant's `CREATE TYPE … AS STRUCT(…)` is
/// produced by `UnionStructPayload::sql_struct_fields`, i.e. by a
/// *different* derive from the one that emits the statement around it. The
/// two agreeing is therefore a property of the pair, and this is the only
/// place it is checked. If they drift, the DDL declares a member list the
/// codec does not write — and because a Turso record carries only storage
/// classes, that mismatch does not error, it decodes into the wrong fields.
#[test]
fn a_payload_declares_the_struct_the_enum_emits() {
    assert_eq!(
        FatPayload::FIELD_NAMES,
        &[
            "account",
            "thread",
            "subject",
            "snippet",
            "sender",
            "recipient",
            "received_at",
            "labels",
        ]
    );

    // Storage classes come from each field's SQL type, so `Option<String>`
    // is TEXT (nullability is not part of a STRICT column class) and the
    // i64 is INT.
    assert_eq!(
        FatPayload::sql_struct_fields(),
        vec![
            ("account", "TEXT"),
            ("thread", "TEXT"),
            ("subject", "TEXT"),
            ("snippet", "TEXT"),
            ("sender", "TEXT"),
            ("recipient", "TEXT"),
            ("received_at", "INT"),
            ("labels", "TEXT"),
        ]
    );

    // The enum's DDL is that list, under the name the variant asked for.
    let sql = BoxedKey::create_type_sql();
    assert!(
        sql.contains(
            "CREATE TYPE boxed_fat_t AS STRUCT(account TEXT, thread TEXT, subject TEXT, \
             snippet TEXT, sender TEXT, recipient TEXT, received_at INT, labels TEXT);"
        ),
        "{sql}"
    );
    assert!(
        sql.contains("CREATE TYPE boxed_key AS UNION(fat boxed_fat_t, legacy INT)"),
        "{sql}"
    );

    // And it is member-for-member the inline twin's, which is the claim
    // the whole boxed/unboxed comparison rests on.
    let inline_sql = InlineKey::create_type_sql();
    assert_eq!(
        sql.replace("boxed_fat_t", "T").replace("boxed_key", "U"),
        inline_sql
            .replace("inline_fat_t", "T")
            .replace("inline_key", "U"),
        "boxed and inline DDL differ by more than the type names"
    );

    // `variant_fields` reaches through the payload too — it is what
    // renderers (`fifteen-cli db`) print a stored row's field names from,
    // so a boxed variant showing up as fieldless would be a silent
    // regression in the tooling rather than in the codec.
    assert_eq!(
        BoxedKey::variant_fields(),
        &[FatPayload::FIELD_NAMES, &[] as &[&str]]
    );
}

/// The payload's own encode/decode pair, exercised without the enum around
/// it.
///
/// `encode_fields` / `decode_fields` are the seam the boxed variant
/// delegates across, and they are position-based: a field list that
/// encodes in one order and decodes in another is wrong in a way no type
/// checks. Round-tripping a value whose fields are all distinguishable
/// (every string different, the optional one set) is what makes a
/// transposition visible.
#[test]
fn a_payload_round_trips_through_its_own_field_codec() -> Result<()> {
    let p = FatPayload {
        account: "a".into(),
        thread: "b".into(),
        subject: "c".into(),
        snippet: "d".into(),
        sender: "e".into(),
        recipient: "f".into(),
        received_at: i64::MIN,
        labels: Some("g".into()),
    };

    let values = p.encode_fields().map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(values.len(), FatPayload::FIELD_NAMES.len());
    assert_eq!(values[0], turso::Value::Text("a".into()));
    assert_eq!(values[6], turso::Value::Integer(i64::MIN));
    assert_eq!(values[7], turso::Value::Text("g".into()));

    assert_eq!(FatPayload::decode_fields(values)?, p);

    // `None` is a stored NULL, not an absent field: the record still has
    // eight columns, because the field count is how the decoder tells a
    // well-formed record from a truncated one.
    let none = FatPayload {
        labels: None,
        ..p.clone()
    };
    let values = none.encode_fields().map_err(|e| anyhow::anyhow!("{e}"))?;
    assert_eq!(values.len(), 8);
    assert_eq!(values[7], turso::Value::Null);
    assert_eq!(FatPayload::decode_fields(values)?, none);

    // A record of the wrong width is refused rather than padded or
    // truncated — the guard that turns a drifted DDL into an error instead
    // of a shifted read.
    let err = FatPayload::decode_fields(vec![turso::Value::Null; 7])
        .expect_err("a 7-field record is not a FatPayload");
    let msg = err.to_string();
    assert!(
        msg.contains("expected 8 fields, got 7"),
        "unhelpful field-count error: {msg}"
    );

    Ok(())
}

// ── 3. `Nullable<TaggedUnion<E>>` ────────────────────────────────────────

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::InlineKey;

    /// A union column that is allowed to be absent — every other table in
    /// this suite declares its union column NOT NULL.
    maybe_rows(id) {
        id -> BigInt,
        k -> Nullable<TaggedUnion<InlineKey>>,
    }
}

async fn maybe_setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&InlineKey::create_type_sql()).await?;
    conn.batch_execute("CREATE TABLE maybe_rows(id INTEGER PRIMARY KEY, k inline_key) STRICT")
        .await?;
    diesel::insert_into(maybe_rows::table)
        .values(vec![
            (
                maybe_rows::id.eq(1i64),
                maybe_rows::k.eq(Some(sample_inline())),
            ),
            (maybe_rows::id.eq(2i64), maybe_rows::k.eq(None::<InlineKey>)),
            (
                maybe_rows::id.eq(3i64),
                maybe_rows::k.eq(Some(InlineKey::Legacy(11))),
            ),
        ])
        .execute(&mut conn)
        .await?;
    Ok(conn)
}

/// A nullable union column reads back as `Option<E>`, and the NULL row is
/// a `None` rather than a decode error.
///
/// The failure this prevents is at the codec boundary: `decode_from_blob`
/// rejects anything that is not a BLOB, so if a nullable column's NULL ever
/// reached it instead of being intercepted by diesel's `Option<T>` impl,
/// every row with an absent union would come back as
/// `DeserializationError("expected UNION blob, got NULL")` — which reads
/// like data corruption and is not.
#[tokio::test(flavor = "current_thread")]
async fn a_nullable_union_column_reads_as_an_option() -> Result<()> {
    let mut conn = maybe_setup().await?;

    let rows: Vec<(i64, Option<InlineKey>)> = maybe_rows::table
        .order(maybe_rows::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(
        rows,
        vec![
            (1, Some(sample_inline())),
            (2, None),
            (3, Some(InlineKey::Legacy(11))),
        ]
    );

    // `IS NULL` on the column is the column's own question, and it is a
    // different question from any tag test.
    let absent: Vec<i64> = maybe_rows::table
        .filter(maybe_rows::k.is_null())
        .select(maybe_rows::id)
        .load(&mut conn)
        .await?;
    assert_eq!(absent, vec![2]);

    Ok(())
}

/// `.extract(…)` over a nullable column, and the two different situations
/// that reach the same `None`.
///
/// A `NULL` out of `union_extract` means one of two things:
///
/// 1. the **column** was NULL — there is no union value here at all; or
/// 2. the column held a value **of another variant** — there is a union
///    value, it is just not this one.
///
/// SQL collapses both to NULL and the DSL faithfully reports both as
/// `None`, so a caller that needs to tell them apart has to ask the column
/// separately (`k IS NULL`). That is not a defect to be papered over — it
/// is why `Extract` is nullable in the first place — but it is a real trap:
/// "rows that are not telegram" and "rows that have no key" are the same
/// answer here, and a filter written as `extract(...).is_null()` quietly
/// includes the empty rows. The assertions below pin both readings side by
/// side so the distinction is written down somewhere executable.
#[tokio::test(flavor = "current_thread")]
async fn extract_over_a_nullable_column_conflates_two_nulls() -> Result<()> {
    use inline_key::fat;

    let mut conn = maybe_setup().await?;

    // Row 1 is a `fat`; row 2 has no value at all; row 3 is a `legacy`.
    // Rows 2 and 3 are indistinguishable through the extract.
    let subjects: Vec<(i64, Option<String>)> = maybe_rows::table
        .order(maybe_rows::id.asc())
        .select((
            maybe_rows::id,
            maybe_rows::k.extract(fat::variant).field(fat::subject),
        ))
        .load(&mut conn)
        .await?;
    assert_eq!(
        subjects,
        vec![
            (1, Some("Re: the wire format".into())),
            (2, None), // NULL column
            (3, None), // present, but tagged `legacy`
        ]
    );

    // `is_not_null()` is still the tag test, and it correctly excludes the
    // NULL column — a row with no union value is not a `fat`.
    let fats: Vec<i64> = maybe_rows::table
        .filter(maybe_rows::k.extract(fat::variant).is_not_null())
        .select(maybe_rows::id)
        .order(maybe_rows::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(fats, vec![1]);

    // The inverse is the trap: "not a fat" picks up the empty row too.
    // Distinguishing them takes a second predicate on the column itself.
    let not_fat: Vec<i64> = maybe_rows::table
        .filter(maybe_rows::k.extract(fat::variant).is_null())
        .select(maybe_rows::id)
        .order(maybe_rows::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(not_fat, vec![2, 3]);

    let not_fat_but_present: Vec<i64> = maybe_rows::table
        .filter(maybe_rows::k.extract(fat::variant).is_null())
        .filter(maybe_rows::k.is_not_null())
        .select(maybe_rows::id)
        .order(maybe_rows::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(not_fat_but_present, vec![3]);

    Ok(())
}

// ── 4. `ValidGrouping` on the expression nodes ───────────────────────────

/// An aggregate over an extracted field, with no `GROUP BY` at all.
///
/// This is `ValidGrouping<()>` on `Extract` and `GetField`: the nodes have
/// to report themselves as *not* aggregate so that wrapping one in `MAX`
/// is legal. They delegate to the operand — `union_extract` is a scalar
/// function, so it neither introduces nor swallows an aggregate — and if
/// that delegation were ever replaced by a fixed `IsAggregate = Yes`, the
/// symptom would be `MAX(struct_extract(...))` failing to compile with a
/// trait error pointing at diesel's internals rather than at the query.
#[tokio::test(flavor = "current_thread")]
async fn an_aggregate_can_wrap_an_extracted_field() -> Result<()> {
    use boxed_key::fat;
    use diesel::dsl::{count_star, max, min};

    let mut conn = setup().await?;
    diesel::insert_into(boxed_rows::table)
        .values(vec![
            (
                boxed_rows::id.eq(1i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(FatPayload {
                    received_at: 10,
                    ..sample_payload()
                }))),
            ),
            (
                boxed_rows::id.eq(2i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(FatPayload {
                    received_at: 30,
                    ..sample_payload()
                }))),
            ),
            (
                boxed_rows::id.eq(3i64),
                boxed_rows::k.eq(BoxedKey::Legacy(1)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let (lo, hi, n): (Option<i64>, Option<i64>, i64) = boxed_rows::table
        .select((
            min(boxed_rows::k
                .extract(fat::variant)
                .field(fat_payload::received_at)),
            max(boxed_rows::k
                .extract(fat::variant)
                .field(fat_payload::received_at)),
            count_star(),
        ))
        .get_result(&mut conn)
        .await?;
    // The `legacy` row extracts to NULL, and MIN/MAX skip NULLs — so the
    // aggregate is over the two `fat` rows while the count is over all
    // three.
    assert_eq!((lo, hi, n), (Some(10), Some(30), 3));

    Ok(())
}

/// `GROUP BY union_tag(col)`, which is the grouping the nodes were given
/// `ValidGrouping` impls for.
///
/// Two things are being pinned. First that the node renders into a
/// `GROUP BY` position at all — it is a `QueryFragment` like any other, but
/// nothing else in the suite puts one anywhere except a `SELECT`, a
/// `WHERE` or an `ORDER BY`. Second that a query grouped this way can be
/// loaded as pure aggregates, which is the narrower half of what
/// `IsContainedInGroupBy` decides; selecting the tag beside them is
/// `group_by_union_tag_selects_the_tag` below.
#[tokio::test(flavor = "current_thread")]
async fn group_by_union_tag_counts_the_variants() -> Result<()> {
    use diesel::dsl::count_star;

    let mut conn = setup().await?;
    diesel::insert_into(boxed_rows::table)
        .values(vec![
            (
                boxed_rows::id.eq(1i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(sample_payload()))),
            ),
            (
                boxed_rows::id.eq(2i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(FatPayload {
                    thread: "other".into(),
                    ..sample_payload()
                }))),
            ),
            (
                boxed_rows::id.eq(3i64),
                boxed_rows::k.eq(BoxedKey::Legacy(1)),
            ),
            (
                boxed_rows::id.eq(4i64),
                boxed_rows::k.eq(BoxedKey::Legacy(2)),
            ),
            (
                boxed_rows::id.eq(5i64),
                boxed_rows::k.eq(BoxedKey::Legacy(3)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let query = boxed_rows::table
        .group_by(boxed_rows::k.union_tag())
        .select(count_star())
        .order(boxed_rows::k.union_tag().asc());
    assert!(
        diesel::debug_query::<diesel::turso::Turso, _>(&query)
            .to_string()
            .contains(r#"GROUP BY union_tag("boxed_rows"."k")"#),
        "{}",
        diesel::debug_query::<diesel::turso::Turso, _>(&query)
    );

    // Tag order is alphabetical here ('fat' < 'legacy'), which is also the
    // declaration order — deliberately not relied on beyond the counts.
    let counts: Vec<i64> = query.load(&mut conn).await?;
    assert_eq!(counts, vec![2, 3]);

    Ok(())
}

/// `GROUP BY union_tag(col)` selecting the tag it grouped by — the query
/// anyone writing the previous one actually wanted.
///
/// Diesel decides "may this expression appear beside aggregates" through
/// `IsContainedInGroupBy`, which `table!` implements for columns and which
/// nothing implemented for these nodes: `k: ValidGrouping<UnionTag<k>>`
/// wants `UnionTag<k>: IsContainedInGroupBy<k>`, and without it
/// `.select((k.union_tag(), count_star()))` was a compile error while
/// `.select(count_star())` over the same `GROUP BY` was fine. That was a
/// gap in the nodes, not a fact about SQL — Turso runs this query — so
/// `Extract`, `GetField` and `UnionTag` each forward the question to their
/// operand, and a group over any of them admits the others.
///
/// The `ORDER BY` is deliberately the grouped expression rather than an
/// ordinal, so the same verdict has to hold in a third clause.
#[tokio::test(flavor = "current_thread")]
async fn group_by_union_tag_selects_the_tag() -> Result<()> {
    use boxed_key::fat;
    use diesel::dsl::count_star;

    let mut conn = setup().await?;
    diesel::insert_into(boxed_rows::table)
        .values(vec![
            (
                boxed_rows::id.eq(1i64),
                boxed_rows::k.eq(BoxedKey::Fat(Box::new(sample_payload()))),
            ),
            (
                boxed_rows::id.eq(2i64),
                boxed_rows::k.eq(BoxedKey::Legacy(1)),
            ),
            (
                boxed_rows::id.eq(3i64),
                boxed_rows::k.eq(BoxedKey::Legacy(2)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let query = boxed_rows::table
        .group_by(boxed_rows::k.union_tag())
        .select((boxed_rows::k.union_tag(), count_star()))
        .order(boxed_rows::k.union_tag().asc());
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(
        sql.contains(r#"GROUP BY union_tag("boxed_rows"."k")"#),
        "{sql}"
    );

    let rows: Vec<(String, i64)> = query.load(&mut conn).await?;
    assert_eq!(
        rows,
        vec![("fat".to_string(), 1), ("legacy".to_string(), 2)]
    );

    // The same for a group over an `Extract`: the extracted payload is
    // selectable beside the aggregate, and so is anything else over the
    // same column.
    let query = boxed_rows::table
        .group_by(boxed_rows::k.extract(fat::variant))
        .select((
            boxed_rows::k
                .extract(fat::variant)
                .field(fat_payload::thread),
            count_star(),
        ))
        .order(boxed_rows::k.union_tag().asc());
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(
        sql.contains(r#"GROUP BY union_extract("boxed_rows"."k", 'fat')"#),
        "{sql}"
    );
    let rows: Vec<(Option<String>, i64)> = query.load(&mut conn).await?;
    assert_eq!(rows, vec![(Some("thread-1".to_string()), 1), (None, 2)]);

    Ok(())
}

/// The grouping that *does* hand the tag back: `GROUP BY` the union column
/// itself.
///
/// Grouping by a column makes every expression over that column
/// functionally determined by the group, which is exactly what
/// `IsContainedInGroupBy<col> for col` says — so `union_tag(col)` and
/// `struct_extract(union_extract(col, …), …)` both become selectable, and
/// the `ValidGrouping` delegation on all three node types is what carries
/// that verdict up from the column. This is the useful form in practice:
/// the union column is a primary key in the app's schema, so grouping by
/// it is grouping by identity, and the duplicate rows below are what a
/// query like this would be looking for.
#[tokio::test(flavor = "current_thread")]
async fn grouping_by_the_column_admits_the_tag() -> Result<()> {
    use diesel::dsl::count_star;
    use inline_key::fat;

    let mut conn = setup().await?;
    // Two rows carrying the *same* union value, plus a third carrying a
    // different one, so the groups have different sizes.
    diesel::insert_into(inline_rows::table)
        .values(vec![
            (inline_rows::id.eq(1i64), inline_rows::k.eq(sample_inline())),
            (inline_rows::id.eq(2i64), inline_rows::k.eq(sample_inline())),
            (
                inline_rows::id.eq(3i64),
                inline_rows::k.eq(InlineKey::Legacy(7)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let query = inline_rows::table
        .group_by(inline_rows::k)
        .select((
            inline_rows::k.union_tag(),
            inline_rows::k.extract(fat::variant).field(fat::received_at),
            count_star(),
        ))
        .order(inline_rows::k.union_tag().asc());
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(sql.contains(r#"GROUP BY "inline_rows"."k""#), "{sql}");
    assert!(
        sql.contains(r#"struct_extract(union_extract("inline_rows"."k", 'fat'), 'received_at')"#),
        "{sql}"
    );

    let rows: Vec<(String, Option<i64>, i64)> = query.load(&mut conn).await?;
    assert_eq!(
        rows,
        vec![
            ("fat".to_string(), Some(0), 2),
            ("legacy".to_string(), None, 1),
        ]
    );

    Ok(())
}

// ── 5. tag ordinals, and what a disagreement about them looks like ───────

/// The truth: two variants, both carrying an `i64`, so nothing about their
/// payloads can tell them apart. (Named `alpha`/`beta` rather than
/// `first`/`second` on purpose — see
/// `a_reserved_word_tag_is_stored_quoted_and_breaks_extract`.)
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Ordinals>)]
#[union(name = "ordinals")]
pub enum Ordinals {
    Alpha(i64),
    Beta(i64),
}

/// The same DDL type, read by an enum whose variants are declared the other
/// way round. Never used to emit DDL — only to decode rows the truthful
/// enum wrote.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Swapped>)]
#[union(name = "swapped")]
pub enum Swapped {
    Beta(i64),
    Alpha(i64),
}

/// The same DDL type again, read by an enum that has never heard of the
/// second variant.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Narrow>)]
#[union(name = "narrow")]
pub enum Narrow {
    Alpha(i64),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Ordinals;

    ordinal_rows(id) {
        id -> BigInt,
        k -> TaggedUnion<Ordinals>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Swapped;

    #[sql_name = "ordinal_rows"]
    ordinal_rows_swapped(id) {
        id -> BigInt,
        k -> TaggedUnion<Swapped>,
    }
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Narrow;

    #[sql_name = "ordinal_rows"]
    ordinal_rows_narrow(id) {
        id -> BigInt,
        k -> TaggedUnion<Narrow>,
    }
}

async fn ordinal_setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&Ordinals::create_type_sql()).await?;
    conn.batch_execute(
        "CREATE TABLE ordinal_rows(id INTEGER PRIMARY KEY, k ordinals NOT NULL) STRICT",
    )
    .await?;
    diesel::insert_into(ordinal_rows::table)
        .values(vec![
            (
                ordinal_rows::id.eq(1i64),
                ordinal_rows::k.eq(Ordinals::Alpha(10)),
            ),
            (
                ordinal_rows::id.eq(2i64),
                ordinal_rows::k.eq(Ordinals::Beta(20)),
            ),
        ])
        .execute(&mut conn)
        .await?;
    Ok(conn)
}

/// The ordinal is the declaration index, and the server agrees.
///
/// `tag_index()` is a Rust-side claim; `union_tag()` is the database
/// resolving the same value against the DDL it was given. Asserting them
/// together is what makes the claim about the *stored* representation
/// rather than about the enum in isolation, and it is the cheap half of
/// what `verify_declared_types` does at establish time.
#[tokio::test(flavor = "current_thread")]
async fn tag_ordinals_are_declaration_indices_on_both_sides() -> Result<()> {
    let mut conn = ordinal_setup().await?;

    assert_eq!(Ordinals::Alpha(0).tag_index(), 0);
    assert_eq!(Ordinals::Beta(0).tag_index(), 1);
    assert_eq!(Ordinals::variants(), &["alpha", "beta"]);

    // The first byte of the stored blob is that index.
    let mut stmt = conn
        .raw()
        .prepare("SELECT k FROM ordinal_rows WHERE id = ?")
        .await?;
    for (id, expected) in [(1i64, 0u8), (2, 1)] {
        let mut rows = stmt.query(vec![turso::Value::Integer(id)]).await?;
        let row = rows.next().await?.expect("row");
        let turso::Value::Blob(b) = row.get_value(0)? else {
            anyhow::bail!("union column is not a blob");
        };
        assert_eq!(b[0], expected, "row {id}");
    }

    // And the server resolves the same index to the same name.
    let tags: Vec<String> = ordinal_rows::table
        .order(ordinal_rows::id.asc())
        .select(ordinal_rows::k.union_tag())
        .load(&mut conn)
        .await?;
    assert_eq!(tags, vec!["alpha", "beta"]);

    Ok(())
}

/// Two enums that disagree about variant order decode each other's rows
/// into the wrong variant, and nothing errors.
///
/// This is the failure mode the whole module is organised around, written
/// down as an executable fact rather than as a warning in a doc comment.
/// The wire carries a tag *index*, never a name; `Swapped` reads index 1
/// and hands back `First`, which is a different value than was stored and
/// is indistinguishable from a correct read. Both payloads are `i64`, so
/// the type system has nothing to object to.
///
/// The point is not that this is a bug in the codec — positional encoding
/// is the format — but that it is why a reordering of variants can never be
/// caught here, and has to be caught by the golden DDL tests and by
/// `verify_declared_types` at establish time. A test that only ever reads
/// with the enum that wrote would leave that argument unsupported.
#[tokio::test(flavor = "current_thread")]
async fn a_reordered_enum_reads_the_wrong_variant_silently() -> Result<()> {
    let mut conn = ordinal_setup().await?;

    let truth: Vec<Ordinals> = ordinal_rows::table
        .order(ordinal_rows::id.asc())
        .select(ordinal_rows::k)
        .load(&mut conn)
        .await?;
    assert_eq!(truth, vec![Ordinals::Alpha(10), Ordinals::Beta(20)]);

    let drifted: Vec<Swapped> = ordinal_rows_swapped::table
        .order(ordinal_rows_swapped::id.asc())
        .select(ordinal_rows_swapped::k)
        .load(&mut conn)
        .await?;
    // Index 0 was written as `Alpha`; `Swapped` calls index 0 `Beta`.
    // The values survive, the variants do not, and there is no error.
    assert_eq!(drifted, vec![Swapped::Beta(10), Swapped::Alpha(20)]);

    Ok(())
}

/// A tag index the reading enum has no variant for is the one drift that
/// *is* caught, and it surfaces as a diesel deserialization error rather
/// than a panic.
///
/// This is the older-binary case: a row written by a build that knew about
/// a variant, read by one that does not. There is no variant to guess at,
/// so `decode` can refuse — and the error names both the offending index
/// and the variants this enum does have, which is the information needed to
/// work out which side is behind.
#[tokio::test(flavor = "current_thread")]
async fn an_unknown_tag_index_is_an_error_not_a_guess() -> Result<()> {
    let mut conn = ordinal_setup().await?;

    // Row 1 is index 0, which `Narrow` does know.
    let ok: Narrow = ordinal_rows_narrow::table
        .find(1i64)
        .select(ordinal_rows_narrow::k)
        .first(&mut conn)
        .await?;
    assert_eq!(ok, Narrow::Alpha(10));

    // Row 2 is index 1, which it does not.
    let err = ordinal_rows_narrow::table
        .find(2i64)
        .select(ordinal_rows_narrow::k)
        .first::<Narrow>(&mut conn)
        .await
        .expect_err("index 1 is out of range for a one-variant enum");
    assert!(
        matches!(err, diesel::result::Error::DeserializationError(_)),
        "{err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("unknown union variant") && msg.contains("alpha"),
        "the error should name the index and the variants that do exist: {msg}"
    );

    Ok(())
}

// ── 6. identifier quoting ────────────────────────────────────────────────

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::InlineKey;

    /// A table and a column whose SQL names contain the one character the
    /// quoting rule exists for. `table!` takes Rust identifiers for the
    /// *paths*, but `#[sql_name = "…"]` sets the SQL name independently,
    /// and that is a plain string literal — so a double quote goes through
    /// the real `table!` path rather than needing a hand-built
    /// `QueryBuilder`.
    #[sql_name = "quo\"ted"]
    quoted_rows(id) {
        id -> BigInt,
        #[sql_name = "we\"ird"]
        weird -> Text,
        k -> TaggedUnion<InlineKey>,
    }
}

/// A union whose first variant's tag is a SQL keyword. Nothing about the
/// Rust spelling says so — `First` snake_cases to `first`, which is a
/// perfectly ordinary identifier everywhere except inside a Turso
/// `CREATE TYPE`.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Keyworded>)]
#[union(name = "keyworded")]
pub enum Keyworded {
    First(i64),
    Second(i64),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::Keyworded;

    keyword_rows(id) {
        id -> BigInt,
        k -> TaggedUnion<Keyworded>,
    }
}

/// A union whose struct variant has a field named for a SQL keyword. `end`
/// is the Rust field name and nothing more was written; `span` and `plain`
/// are ordinary names, so the only thing under test here is the field.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<KeywordFields>)]
#[union(name = "keyword_fields")]
pub enum KeywordFields {
    #[union(struct_type = "span_t")]
    Span {
        end: i64,
        note: String,
    },
    Plain(i64),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::KeywordFields;

    field_rows(id) {
        id -> BigInt,
        k -> TaggedUnion<KeywordFields>,
    }
}

/// `push_identifier` doubles an embedded double quote, and nothing in the
/// suite had ever handed it one.
///
/// SQLite (and Turso) escape a `"` inside a quoted identifier by doubling
/// it, so `we"ird` has to render as `"we""ird"`. Get that wrong and the
/// statement does not merely name the wrong column — the quote terminates
/// the identifier early and the rest of the name becomes stray syntax, so
/// the query fails to parse. That is a loud failure, which is the good
/// case; the reason to have a test is that the code path is a branch taken
/// by roughly no identifier in production, so it can rot for years and then
/// be reached by the first user-supplied name that has a quote in it.
///
/// Both halves are asserted: the rendered SQL (so the doubling is pinned
/// even if Turso were ever lenient about it) and a real round-trip through
/// the database (so the pinning is of something that actually works).
#[tokio::test(flavor = "current_thread")]
async fn identifiers_with_embedded_quotes_are_doubled() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&InlineKey::create_type_sql()).await?;
    conn.batch_execute(
        "CREATE TABLE \"quo\"\"ted\"(\
            id INTEGER PRIMARY KEY, \
            \"we\"\"ird\" TEXT NOT NULL, \
            k inline_key NOT NULL\
         ) STRICT",
    )
    .await?;

    let insert = diesel::insert_into(quoted_rows::table).values((
        quoted_rows::id.eq(1i64),
        quoted_rows::weird.eq("value"),
        quoted_rows::k.eq(InlineKey::Legacy(3)),
    ));
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&insert).to_string();
    assert!(sql.contains(r#""quo""ted""#), "{sql}");
    assert!(sql.contains(r#""we""ird""#), "{sql}");
    insert.execute(&mut conn).await?;

    let query = quoted_rows::table
        .filter(quoted_rows::weird.eq("value"))
        .select((quoted_rows::weird, quoted_rows::k));
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(sql.contains(r#""quo""ted"."we""ird""#), "{sql}");

    let got: (String, InlineKey) = query.first(&mut conn).await?;
    assert_eq!(got, ("value".to_string(), InlineKey::Legacy(3)));

    // The quoted identifier survives a union expression too, where the
    // column reference is nested two function calls deep.
    let tags: Vec<String> = quoted_rows::table
        .select(quoted_rows::k.union_tag())
        .load(&mut conn)
        .await?;
    assert_eq!(tags, vec!["legacy"]);

    Ok(())
}

/// The other half of the quoting story: a variant whose tag is a SQL
/// keyword, and the spelling that makes it work.
///
/// `CREATE TYPE … AS UNION(first INT, second INT)` is accepted, but Turso
/// re-renders the statement from its own AST before persisting it and
/// quotes any member name that would not lex back as a plain identifier.
/// `first` is a keyword, so `sqlite_turso_types` holds
/// `AS UNION("first" INT, second INT)` — and the two quote characters are
/// then *part of the variant's name*. `union_tag(col)` returns the
/// seven-character string `"first"`; `union_extract(col, 'first')` is not a
/// NULL but `Parse error: cannot resolve union variant 'first'`;
/// `union_value('first', …)` fails the same way, so a migration cannot even
/// write the variant.
///
/// None of that shows up on the whole-value path, because our `ToSql` binds
/// the wire blob and never names the tag: inserts, selects and round-trips
/// are all fine and the tag ordinal is untouched. So a union with a keyword
/// variant looks completely healthy right up until somebody writes a tag
/// test, and then it is a runtime parse error rather than a compile error
/// or a wrong answer. `First`, `Last`, `Next`, `Key`, `Order`, `Window`,
/// `End` — the derive snake_cases the ident, so no string has to be typed
/// for a union to land here.
///
/// The fix is that `#[derive(UnionSchema)]` spells every name the way Turso
/// will store it, computed at expansion time against Turso's own keyword
/// list, and emits that one spelling as `TAG_NAME`, as the literal
/// [`Extract`] pushes, and as the DDL member name. So this test asserts the
/// three agree with each other *and* with the database:
///
/// * the DDL the derive emits is byte-identical to the one Turso kept;
/// * `union_extract` and a tag test resolve the keyword variant;
/// * `union_tag(col)` compares equal to `TAG_NAME`.
///
/// The `second` variant is carried alongside throughout as the control: a
/// name needing no quoting renders exactly as it always did, which is the
/// property the ~60 tag call sites in the app depend on.
///
/// [`Extract`]: diesel::turso::union::Extract
#[tokio::test(flavor = "current_thread")]
async fn a_reserved_word_tag_is_named_the_way_turso_stores_it() -> Result<()> {
    use keyworded::{first, second};

    // The two spellings, before anything touches a database: the keyword
    // carries the quotes, its neighbour does not.
    assert_eq!(first::TAG_NAME, r#""first""#);
    assert_eq!(second::TAG_NAME, "second");
    assert_eq!(Keyworded::variants(), &[r#""first""#, "second"]);

    let mut conn = TursoConnection::establish(":memory:").await?;
    let derived = Keyworded::create_type_sql();
    assert_eq!(
        derived,
        r#"CREATE TYPE keyworded AS UNION("first" INT, second INT)"#,
    );
    conn.batch_execute(&derived).await?;
    conn.batch_execute(
        "CREATE TABLE keyword_rows(id INTEGER PRIMARY KEY, k keyworded NOT NULL) STRICT",
    )
    .await?;

    // Turso hands the declaration back unchanged, which is the point: the
    // derive already wrote it in the engine's spelling, so there is nothing
    // for `check_declarations` to report and nothing for a later reader to
    // be surprised by.
    let mut rows = conn
        .raw()
        .query(
            "SELECT sql FROM sqlite_turso_types WHERE name = 'keyworded'",
            (),
        )
        .await?;
    let decl = match rows
        .next()
        .await?
        .expect("the type was created")
        .get_value(0)?
    {
        turso::Value::Text(s) => s,
        other => anyhow::bail!("declaration came back as {other:?}"),
    };
    assert_eq!(decl, derived, "turso stores what the derive emitted");

    diesel::insert_into(keyword_rows::table)
        .values(vec![
            (
                keyword_rows::id.eq(1i64),
                keyword_rows::k.eq(Keyworded::First(10)),
            ),
            (
                keyword_rows::id.eq(2i64),
                keyword_rows::k.eq(Keyworded::Second(20)),
            ),
        ])
        .execute(&mut conn)
        .await?;
    let got: Vec<Keyworded> = keyword_rows::table
        .order(keyword_rows::id.asc())
        .select(keyword_rows::k)
        .load(&mut conn)
        .await?;
    assert_eq!(got, vec![Keyworded::First(10), Keyworded::Second(20)]);

    // `union_tag` and `TAG_NAME` now name the same string, so the obvious
    // comparison selects the row it reads as selecting.
    let tags: Vec<String> = keyword_rows::table
        .order(keyword_rows::id.asc())
        .select(keyword_rows::k.union_tag())
        .load(&mut conn)
        .await?;
    assert_eq!(tags, vec![r#""first""#, "second"]);
    let matched: Vec<i64> = keyword_rows::table
        .filter(keyword_rows::k.union_tag().eq(first::TAG_NAME))
        .select(keyword_rows::id)
        .load(&mut conn)
        .await?;
    assert_eq!(matched, vec![1]);

    // `Extract` renders the quoted form, so the statement parses and the
    // tag test answers rather than failing.
    let query = keyword_rows::table
        .filter(keyword_rows::k.extract(first::variant).is_not_null())
        .select(keyword_rows::id);
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(
        sql.contains(r#"union_extract("keyword_rows"."k", '"first"')"#),
        "{sql}"
    );
    let ids: Vec<i64> = query.load(&mut conn).await?;
    assert_eq!(ids, vec![1]);

    // And the payload comes back, so the quoting is resolving the variant
    // rather than merely parsing.
    let payloads: Vec<Option<i64>> = keyword_rows::table
        .order(keyword_rows::id.asc())
        .select(keyword_rows::k.extract(first::variant))
        .load(&mut conn)
        .await?;
    assert_eq!(payloads, vec![Some(10), None]);

    // The control: a name that needs no quoting renders with none, exactly
    // as every tag in the app does.
    let query = keyword_rows::table
        .filter(keyword_rows::k.extract(second::variant).is_not_null())
        .select(keyword_rows::id);
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(
        sql.contains(r#"union_extract("keyword_rows"."k", 'second')"#),
        "{sql}"
    );
    assert_eq!(query.load::<i64>(&mut conn).await?, vec![2]);

    Ok(())
}

/// The same rule, one level down: a STRUCT *field* whose name is a keyword.
///
/// `struct_extract` resolves a field name against the stored STRUCT
/// declaration, which Turso re-renders and requotes by the identical rule —
/// so a field called `end` is stored as `"end"` and
/// `struct_extract(…, 'end')` fails to resolve. Worth its own test because
/// the two names arrive from different places in the derive (a variant tag
/// can be overridden with `#[union(tag = …)]`; a field name is the Rust
/// ident and nothing else), and because the field case is the one nobody
/// would think to check — `end`, `order`, `group`, `key`, `index`, `range`
/// are all perfectly ordinary struct fields.
#[tokio::test(flavor = "current_thread")]
async fn a_reserved_word_field_is_named_the_way_turso_stores_it() -> Result<()> {
    use keyword_fields::span;

    use diesel::turso::union::CompositeField;
    assert_eq!(<span::end as CompositeField>::NAME, r#""end""#);
    assert_eq!(<span::note as CompositeField>::NAME, "note");

    let mut conn = TursoConnection::establish(":memory:").await?;
    let derived = KeywordFields::create_type_sql();
    assert!(
        derived.contains(r#"CREATE TYPE span_t AS STRUCT("end" INT, note TEXT)"#),
        "{derived}"
    );
    conn.batch_execute(&derived).await?;
    conn.batch_execute(
        "CREATE TABLE field_rows(id INTEGER PRIMARY KEY, k keyword_fields NOT NULL) STRICT",
    )
    .await?;

    diesel::insert_into(field_rows::table)
        .values(vec![
            (
                field_rows::id.eq(1i64),
                field_rows::k.eq(KeywordFields::Span {
                    end: 42,
                    note: "done".into(),
                }),
            ),
            (
                field_rows::id.eq(2i64),
                field_rows::k.eq(KeywordFields::Plain(7)),
            ),
        ])
        .execute(&mut conn)
        .await?;

    let query = field_rows::table
        .order(field_rows::id.asc())
        .select(field_rows::k.extract(span::variant).field(span::end));
    let sql = diesel::debug_query::<diesel::turso::Turso, _>(&query).to_string();
    assert!(
        sql.contains(r#"struct_extract(union_extract("field_rows"."k", 'span'), '"end"')"#),
        "{sql}"
    );
    let ends: Vec<Option<i64>> = query.load(&mut conn).await?;
    assert_eq!(ends, vec![Some(42), None]);

    // The neighbouring field is unquoted, and the whole value still decodes.
    let notes: Vec<Option<String>> = field_rows::table
        .order(field_rows::id.asc())
        .select(field_rows::k.extract(span::variant).field(span::note))
        .load(&mut conn)
        .await?;
    assert_eq!(notes, vec![Some("done".to_string()), None]);
    let got: Vec<KeywordFields> = field_rows::table
        .order(field_rows::id.asc())
        .select(field_rows::k)
        .load(&mut conn)
        .await?;
    assert_eq!(
        got,
        vec![
            KeywordFields::Span {
                end: 42,
                note: "done".into()
            },
            KeywordFields::Plain(7),
        ]
    );

    Ok(())
}
