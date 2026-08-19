//! Our UNION encoder against Turso's own, byte for byte.
//!
//! # The invariant, and why it had nothing watching it
//!
//! Two independent implementations write the same bytes. Migrations build
//! UNION values in SQL with `union_value('tag', struct_pack(…))` — 858 such
//! lines in the `self_chat` migration alone — and the app then looks those
//! rows up by binding *our* `encode_union` output against a UNION primary
//! key. Nothing forces the two to agree. They do agree today because
//! `wire::encode_integer` and Turso's `SerialType::from` independently
//! reached the same reading of the SQLite record format, including the two
//! payload-free serials for the literals 0 and 1 — but "independently
//! reached the same reading" is not a property, it is a coincidence with a
//! good track record.
//!
//! When it breaks, nothing errors. A row written by a migration and a row
//! written by the app become two different byte strings for the same
//! logical value, so the app's lookup misses, its insert succeeds, and the
//! table quietly grows a duplicate identity. Integer `0` is the case to
//! worry about most: `MessageId::telegram` normalises `user_id` to
//! `UserId(0)` for every group message, so serial 8 is on the hot path for
//! a large fraction of every chat in the database.
//!
//! # What this covers
//!
//! A spread rather than a handful of hand-picked values: every signed-width
//! boundary the encoder branches on (±2^7, ±2^15, ±2^23, ±2^31, ±2^47,
//! i64 extremes) and both sides of each, the literal serials 0 and 1,
//! floats including the sign-of-zero, subnormal, infinite and NaN cases,
//! text and blobs at the lengths where the header-size varint changes
//! width, and NULL in every slot. NaN earns its place twice over: it is the
//! one value Turso cannot store as a REAL at all, so it is the one case
//! where following IEEE rather than following Turso *is* the divergence
//! this file exists to catch — and it went uncovered here for exactly as
//! long as our encoder got it wrong. Each case runs through three
//! assertions:
//!
//! 1. our bytes equal the bytes Turso wrote for the same value,
//! 2. our bytes decode back to the value we started from,
//! 3. binding our bytes finds the row Turso wrote — the operation whose
//!    silent failure is the reason for the test.

use anyhow::Result;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::turso::union::{decode_record, encode_record, TaggedUnion, UnionSchema};
use diesel::turso::TursoConnection;
use diesel::UnionSchema as DeriveUnionSchema;

/// A struct variant with one slot per storage class, so a case can put a
/// value under test in its own slot and hold the others fixed, and a union
/// with the same classes as bare scalar variants.
const SCHEMA: &str = "
    CREATE TYPE probe_t AS STRUCT(i INT, r REAL, t TEXT, b BLOB);
    CREATE TYPE probe AS UNION(s probe_t, i INT, r REAL, t TEXT, b BLOB);
    CREATE TABLE rows_(id INTEGER PRIMARY KEY, v probe NOT NULL) STRICT;
";

/// Union tag ordinals, matching `SCHEMA`'s declaration order.
const TAG_STRUCT: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BLOB: u8 = 4;

/// Every integer the encoder's width ladder branches on, and its
/// neighbours on both sides. The two literal serials (0, 1) lead because
/// they are the ones that carry no payload at all, and the one that is
/// everywhere in real data.
fn integer_cases() -> Vec<i64> {
    let mut v = vec![0, 1, -1, 2, -2];
    for boundary in [7u32, 15, 23, 31, 47] {
        let edge = 1i64 << boundary;
        v.extend([edge - 1, edge, -edge, -edge - 1, edge + 1]);
    }
    v.extend([i64::MAX, i64::MIN, i64::MAX - 1, i64::MIN + 1]);
    v
}

/// The float edge cases, including the three IEEE values that are not
/// ordinary numbers. The infinities Turso stores as ordinary serial-7
/// doubles; NaN it cannot store at all — `Value::from_f64` folds it to NULL
/// — and leaving NaN out of this list is what let our encoder emit serial 7
/// for it and diverge from Turso for a value the app can perfectly well
/// hand us.
fn real_cases() -> Vec<f64> {
    vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.5,
        -1.5,
        std::f64::consts::PI,
        f64::MIN,
        f64::MAX,
        f64::MIN_POSITIVE,
        f64::EPSILON,
        1e308,
        -1e-308,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
    ]
}

/// The value Turso will hand back for `v`, which is `v` itself except for
/// NaN: `Value::from_f64` has no NaN to return, so a NaN written on either
/// side of the wire reads back as NULL. Round-tripping is asserted against
/// this rather than against the input, because "what we wrote comes back"
/// means what the *database* can hold, and `NaN == NaN` is false in any
/// case.
fn as_turso_stores_it(v: &turso::Value) -> turso::Value {
    match v {
        turso::Value::Real(f) if f.is_nan() => turso::Value::Null,
        other => other.clone(),
    }
}

/// Lengths chosen around the points where a TEXT/BLOB serial type — and
/// through it the header-size varint — crosses a varint width. A 20 KB
/// payload is what once panicked the encoder for real.
fn text_cases() -> Vec<String> {
    let mut v: Vec<String> = ["", "a", "hi", "\0embedded nul", "🙂 Ünïcödé"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    v.extend([56, 57, 58, 8100, 8200, 20_000].map(|n| "x".repeat(n)));
    v
}

fn blob_cases() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![vec![], vec![0], vec![0xff], vec![0xde, 0xad, 0xbe, 0xef]];
    v.extend([56, 57, 58, 8100, 8200, 20_000].map(|n| vec![0xab; n]));
    v
}

/// One case: a label for the failure message, and the four struct fields.
struct Case {
    label: String,
    fields: Vec<turso::Value>,
}

/// The struct-variant matrix. Each case varies one slot and holds the
/// other three at a fixed non-trivial value, so a bug confined to one
/// storage class still shows up next to neighbours that encode correctly.
fn struct_cases() -> Vec<Case> {
    let base = || {
        vec![
            turso::Value::Integer(7),
            turso::Value::Real(1.5),
            turso::Value::Text("base".into()),
            turso::Value::Blob(vec![0x01, 0x02]),
        ]
    };
    let mut cases = Vec::new();
    let mut push = |label: String, slot: usize, value: turso::Value| {
        let mut fields = base();
        fields[slot] = value;
        cases.push(Case { label, fields });
    };
    for n in integer_cases() {
        push(format!("int {n}"), 0, turso::Value::Integer(n));
    }
    for f in real_cases() {
        push(format!("real {f:e}"), 1, turso::Value::Real(f));
    }
    for s in text_cases() {
        push(format!("text len {}", s.len()), 2, turso::Value::Text(s));
    }
    for b in blob_cases() {
        push(format!("blob len {}", b.len()), 3, turso::Value::Blob(b));
    }
    for slot in 0..4 {
        push(format!("null in slot {slot}"), slot, turso::Value::Null);
    }
    cases.push(Case {
        label: "all null".into(),
        fields: vec![turso::Value::Null; 4],
    });
    cases
}

/// The scalar-variant cases: the same value spread, but as the union's
/// lone column rather than inside a struct record. Scalar variants skip
/// the inner record entirely, so they exercise the outer framing on its
/// own.
fn scalar_cases() -> Vec<(u8, Case)> {
    let mut cases = Vec::new();
    for n in integer_cases() {
        cases.push((
            TAG_INT,
            Case {
                label: format!("scalar int {n}"),
                fields: vec![turso::Value::Integer(n)],
            },
        ));
    }
    for f in real_cases() {
        cases.push((
            TAG_REAL,
            Case {
                label: format!("scalar real {f:e}"),
                fields: vec![turso::Value::Real(f)],
            },
        ));
    }
    for s in text_cases() {
        cases.push((
            TAG_TEXT,
            Case {
                label: format!("scalar text len {}", s.len()),
                fields: vec![turso::Value::Text(s)],
            },
        ));
    }
    for b in blob_cases() {
        cases.push((
            TAG_BLOB,
            Case {
                label: format!("scalar blob len {}", b.len()),
                fields: vec![turso::Value::Blob(b)],
            },
        ));
    }
    cases
}

/// Our side of the comparison, spelled out rather than reached through the
/// derive so the test pins the wire functions themselves.
fn our_union_bytes(tag: u8, outer: turso::Value) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(encode_record(&[outer]));
    out
}

fn our_struct_bytes(fields: &[turso::Value]) -> Vec<u8> {
    our_union_bytes(TAG_STRUCT, turso::Value::Blob(encode_record(fields)))
}

async fn setup() -> Result<TursoConnection> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(SCHEMA).await?;
    Ok(conn)
}

/// Read the raw blob Turso stored for row `id`.
async fn stored_bytes(conn: &TursoConnection, id: i64) -> Result<Vec<u8>> {
    let mut stmt = conn
        .raw()
        .prepare("SELECT v FROM rows_ WHERE id = ?")
        .await?;
    let mut rows = stmt.query(vec![turso::Value::Integer(id)]).await?;
    let row = rows.next().await?.expect("row was inserted");
    match row.get_value(0)? {
        turso::Value::Blob(b) => Ok(b),
        other => anyhow::bail!("union column came back as {other:?}, not a blob"),
    }
}

/// Bind our own encoding against the UNION column and ask which row it is.
/// This is the operation the whole test exists for: the app finds a
/// migration-written row by binding its own bytes.
async fn lookup_by_our_bytes(conn: &TursoConnection, bytes: &[u8]) -> Result<Option<i64>> {
    let mut stmt = conn
        .raw()
        .prepare("SELECT id FROM rows_ WHERE v = ?")
        .await?;
    let mut rows = stmt.query(vec![turso::Value::Blob(bytes.to_vec())]).await?;
    match rows.next().await? {
        Some(row) => match row.get_value(0)? {
            turso::Value::Integer(id) => Ok(Some(id)),
            other => anyhow::bail!("id came back as {other:?}"),
        },
        None => Ok(None),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn struct_variants_match_struct_pack() -> Result<()> {
    let conn = setup().await?;
    let cases = struct_cases();
    assert!(cases.len() > 40, "the spread is the point of this test");
    let mut first_id_for_bytes: std::collections::HashMap<Vec<u8>, i64> =
        std::collections::HashMap::new();

    for (i, case) in cases.iter().enumerate() {
        let id = i as i64 + 1;
        let mut binds = vec![turso::Value::Integer(id)];
        binds.extend(case.fields.iter().cloned());
        let mut stmt = conn
            .raw()
            .prepare("INSERT INTO rows_ VALUES (?, union_value('s', struct_pack(?, ?, ?, ?)))")
            .await?;
        stmt.execute(binds).await?;

        let theirs = stored_bytes(&conn, id).await?;
        let ours = our_struct_bytes(&case.fields);
        assert_eq!(
            ours, theirs,
            "{}: our encoding disagrees with union_value/struct_pack",
            case.label
        );

        // And our decoder reads back what we started from — an encoder
        // that agreed with Turso on the wrong bytes would still be wrong.
        let turso::Value::Blob(inner) = decode_record(&theirs[1..])?
            .pop()
            .expect("outer record has one column")
        else {
            anyhow::bail!("{}: outer column was not a blob", case.label);
        };
        let stored: Vec<turso::Value> = case.fields.iter().map(as_turso_stores_it).collect();
        assert_eq!(decode_record(&inner)?, stored, "{}", case.label);

        // Two cases can legitimately share a row identity: NaN and NULL are
        // one value on Turso, so `real NaN` and `null in slot 1` write the
        // same bytes and a lookup answers with whichever landed first. What
        // the assertion is about is that binding our bytes finds *the* row
        // those bytes name, so compare against the first id that wrote them.
        let expected = *first_id_for_bytes.entry(ours.clone()).or_insert(id);
        assert_eq!(
            lookup_by_our_bytes(&conn, &ours).await?,
            Some(expected),
            "{}: binding our bytes did not find the row turso wrote",
            case.label
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_variants_match_union_value() -> Result<()> {
    let conn = setup().await?;
    for (i, (tag, case)) in scalar_cases().iter().enumerate() {
        let id = i as i64 + 1;
        let tag_name = ["s", "i", "r", "t", "b"][*tag as usize];
        let sql = format!("INSERT INTO rows_ VALUES (?, union_value('{tag_name}', ?))");
        let mut stmt = conn.raw().prepare(&sql).await?;
        stmt.execute(vec![turso::Value::Integer(id), case.fields[0].clone()])
            .await?;

        let theirs = stored_bytes(&conn, id).await?;
        let ours = our_union_bytes(*tag, case.fields[0].clone());
        assert_eq!(ours, theirs, "{}", case.label);
        assert_eq!(
            lookup_by_our_bytes(&conn, &ours).await?,
            Some(id),
            "{}: binding our bytes did not find the row turso wrote",
            case.label
        );
    }
    Ok(())
}

/// The hot-path case called out by name, kept as its own test so a
/// regression names itself in the output rather than being case 1 of 60.
///
/// `MessageId::telegram` normalises `user_id` to `UserId(0)` for every
/// group and channel message, so the integer 0 — SQLite serial type 8,
/// which carries no payload byte at all — is in the primary key of a large
/// fraction of the rows in `messages`.
#[tokio::test(flavor = "current_thread")]
async fn integer_zero_and_one_use_the_payload_free_serials() -> Result<()> {
    let conn = setup().await?;
    let mut stmt = conn
        .raw()
        .prepare("INSERT INTO rows_ VALUES (?, union_value('s', struct_pack(?, 1.0, 'x', X'00')))")
        .await?;
    for (id, n) in [(1i64, 0i64), (2, 1)] {
        stmt.execute(vec![turso::Value::Integer(id), turso::Value::Integer(n)])
            .await?;
        let fields = vec![
            turso::Value::Integer(n),
            turso::Value::Real(1.0),
            turso::Value::Text("x".into()),
            turso::Value::Blob(vec![0]),
        ];
        let ours = our_struct_bytes(&fields);
        assert_eq!(ours, stored_bytes(&conn, id).await?, "integer {n}");
        assert_eq!(lookup_by_our_bytes(&conn, &ours).await?, Some(id));
    }
    // Serial 8 / 9 carry no payload: the record for `[0]` is exactly the
    // two-byte header and nothing else.
    assert_eq!(encode_record(&[turso::Value::Integer(0)]), vec![2, 8]);
    assert_eq!(encode_record(&[turso::Value::Integer(1)]), vec![2, 9]);
    Ok(())
}

// -- the derive's own output, against turso ----------------------------------

/// One variant per field-type family the app actually stores, so the whole
/// `TursoFieldType` → `ToSql` path is measured against Turso rather than
/// only the record encoder underneath it.
///
/// This is the assertion that says field conversion moving from the old
/// `FieldCodec` trait onto diesel's `ToSql`/`FromSql` did not move a single
/// byte. A `FieldCodec` impl and a `ToSql` impl for the same Rust type were
/// free to disagree — they were separate code making the same decision
/// twice — and this is the only place that disagreement would have shown up.
#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<Fields>)]
#[union(name = "fields")]
pub enum Fields {
    /// Integers, floats and booleans — every INT/REAL-class conversion.
    #[union(struct_type = "fields_numbers")]
    Numbers {
        small: i16,
        medium: i32,
        big: i64,
        flag: bool,
        single: f32,
        double: f64,
    },
    /// The nullable path: diesel's blanket `Option<T>` impls decide these,
    /// and `None` never touches the bind buffer at all.
    #[union(struct_type = "fields_maybe")]
    Maybe {
        text: Option<String>,
        number: Option<i64>,
        bytes: Option<Vec<u8>>,
    },
    /// BLOB-class conversions, including the `Uuid` alias.
    #[union(struct_type = "fields_bytes")]
    Bytes { raw: Vec<u8>, id: uuid::Uuid },
    /// The one field type whose storage is a per-field decision rather than
    /// a property of the Rust type.
    #[union(struct_type = "fields_lists")]
    Lists {
        #[union(sql_type = diesel::sql_types::Text)]
        labels: Vec<String>,
    },
    /// Scalar variant: no inner record, the outer column is the value.
    Count(i64),
}

const FIELDS_TABLE: &str = "CREATE TABLE derived(id INTEGER PRIMARY KEY, v fields NOT NULL) STRICT";

/// Insert the same logical value twice — once as SQL Turso builds itself,
/// once as bytes the derive produced — and require the two to be identical.
async fn assert_derive_matches(
    conn: &TursoConnection,
    id: i64,
    value: &Fields,
    turso_sql: &str,
    binds: Vec<turso::Value>,
) -> Result<()> {
    let mut all = vec![turso::Value::Integer(id)];
    all.extend(binds);
    let mut stmt = conn.raw().prepare(turso_sql).await?;
    stmt.execute(all).await?;

    let theirs = stored_derived_bytes(conn, id).await?;
    let outer_value = value
        .encode_outer()
        .map_err(|e| anyhow::anyhow!("encode {value:?}: {e}"))?;
    let ours = our_union_bytes(value.tag_index(), outer_value);
    assert_eq!(ours, theirs, "{value:?}");

    // And the derive reads its own bytes back to the value we started from.
    let outer = decode_record(&theirs[1..])?.pop().expect("one column");
    assert_eq!(&Fields::decode(theirs[0], outer)?, value, "{value:?}");
    Ok(())
}

async fn stored_derived_bytes(conn: &TursoConnection, id: i64) -> Result<Vec<u8>> {
    let mut stmt = conn
        .raw()
        .prepare("SELECT v FROM derived WHERE id = ?")
        .await?;
    let mut rows = stmt.query(vec![turso::Value::Integer(id)]).await?;
    let row = rows.next().await?.expect("row was inserted");
    match row.get_value(0)? {
        turso::Value::Blob(b) => Ok(b),
        other => anyhow::bail!("union column came back as {other:?}, not a blob"),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn the_derive_agrees_with_turso_for_every_field_type() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&Fields::create_type_sql()).await?;
    conn.batch_execute(FIELDS_TABLE).await?;

    let uuid = uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);

    assert_derive_matches(
        &conn,
        1,
        &Fields::Numbers {
            // 0 and 1 are the payload-free serials; i64::MIN is the far end
            // of the width ladder.
            small: 0,
            medium: 1,
            big: i64::MIN,
            flag: true,
            single: 0.5,
            double: -1.5,
        },
        "INSERT INTO derived VALUES (?, union_value('numbers', struct_pack(?, ?, ?, ?, ?, ?)))",
        vec![
            turso::Value::Integer(0),
            turso::Value::Integer(1),
            turso::Value::Integer(i64::MIN),
            turso::Value::Integer(1),
            turso::Value::Real(0.5),
            turso::Value::Real(-1.5),
        ],
    )
    .await?;

    assert_derive_matches(
        &conn,
        2,
        &Fields::Maybe {
            text: None,
            number: None,
            bytes: None,
        },
        "INSERT INTO derived VALUES (?, union_value('maybe', struct_pack(?, ?, ?)))",
        vec![turso::Value::Null, turso::Value::Null, turso::Value::Null],
    )
    .await?;

    assert_derive_matches(
        &conn,
        3,
        &Fields::Maybe {
            text: Some("hi".into()),
            number: Some(0),
            bytes: Some(vec![]),
        },
        "INSERT INTO derived VALUES (?, union_value('maybe', struct_pack(?, ?, ?)))",
        vec![
            turso::Value::Text("hi".into()),
            turso::Value::Integer(0),
            turso::Value::Blob(vec![]),
        ],
    )
    .await?;

    assert_derive_matches(
        &conn,
        4,
        &Fields::Bytes {
            raw: vec![0xde, 0xad, 0xbe, 0xef],
            id: uuid,
        },
        "INSERT INTO derived VALUES (?, union_value('bytes', struct_pack(?, ?)))",
        vec![
            turso::Value::Blob(vec![0xde, 0xad, 0xbe, 0xef]),
            turso::Value::Blob(uuid.as_bytes().to_vec()),
        ],
    )
    .await?;

    // The JSON encoding is ours, so the comparison binds the text Turso
    // would have to be handed — which is the point: it pins the exact
    // string, separators and escapes and all.
    assert_derive_matches(
        &conn,
        5,
        &Fields::Lists {
            labels: vec!["INBOX".into(), "a\"b".into()],
        },
        "INSERT INTO derived VALUES (?, union_value('lists', struct_pack(?)))",
        vec![turso::Value::Text(r#"["INBOX","a\"b"]"#.into())],
    )
    .await?;

    assert_derive_matches(
        &conn,
        6,
        &Fields::Count(0),
        "INSERT INTO derived VALUES (?, union_value('count', ?))",
        vec![turso::Value::Integer(0)],
    )
    .await?;

    Ok(())
}
