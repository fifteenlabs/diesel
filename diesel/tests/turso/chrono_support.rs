//! Acceptance tests for the `chrono` feature:
//!
//! * Scalar datetime columns (`Date`, `Time`, `Timestamp`) carrying
//!   `chrono::NaiveDate`, `NaiveTime`, `NaiveDateTime`.
//! * `chrono::Naive*` field types inside UNION variants (through the
//!   `#[derive(UnionSchema)]` path).

#![cfg(feature = "chrono")]

use anyhow::Result;
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use diesel::async_dsl::RunQueryDsl;
use diesel::connection::{AsyncConnection, SimpleAsyncConnection};
use diesel::define_sql_function;
use diesel::deserialize::FromSqlRow;
use diesel::expression::AsExpression;
use diesel::prelude::*;
use diesel::sql_types::Text;
use diesel::turso::union::{TaggedUnion, UnionSchema};
use diesel::turso::TursoConnection;
use diesel::UnionSchema as DeriveUnionSchema;

// ----- Scalar datetime columns --------------------------------------------

diesel::table! {
    events (id) {
        id -> BigInt,
        happened_on -> Date,
        happened_at_time -> Time,
        happened_at -> Timestamp,
    }
}

// A single TEXT column, written as text and read back as a `Timestamp`. Two
// `table!` blocks over one physical table is how a test gets a chosen literal
// in front of `FromSql<Timestamp>`: the write is typed as what it is (a
// string), the read is typed as what the app would declare, and neither side
// is a hand-written statement.
diesel::table! {
    probes (id) {
        id -> BigInt,
        t -> Text,
    }
}

diesel::table! {
    #[sql_name = "probes"]
    probes_as_timestamp (id) {
        id -> BigInt,
        t -> Timestamp,
    }
}

const PROBES_DDL: &str = "CREATE TABLE probes(id INTEGER PRIMARY KEY, t TEXT NOT NULL) STRICT";

// turso's `julianday(...)` returns a REAL. Declaring the return as `Timestamp`
// is what routes that REAL into our `FromSql<Timestamp>` numeric fallback —
// the same declaration an app would write to use the function.
define_sql_function! {
    fn julianday(t: Text) -> Timestamp;
}

#[derive(Insertable, Queryable, Debug, PartialEq)]
#[diesel(table_name = events)]
struct Event {
    id: i64,
    happened_on: NaiveDate,
    happened_at_time: NaiveTime,
    happened_at: NaiveDateTime,
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_datetime_roundtrip() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE events(
            id INTEGER PRIMARY KEY,
            happened_on DATE NOT NULL,
            happened_at_time TIME NOT NULL,
            happened_at TIMESTAMP NOT NULL
        ) STRICT",
    )
    .await?;

    let row = Event {
        id: 1,
        happened_on: NaiveDate::from_ymd_opt(2026, 4, 13).unwrap(),
        happened_at_time: NaiveTime::from_hms_opt(15, 30, 45).unwrap(),
        happened_at: NaiveDate::from_ymd_opt(2026, 4, 13)
            .unwrap()
            .and_hms_opt(15, 30, 45)
            .unwrap(),
    };
    diesel::insert_into(events::table)
        .values(&row)
        .execute(&mut conn)
        .await?;

    let got: Event = events::table.find(1i64).first(&mut conn).await?;
    assert_eq!(got, row);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scalar_datetime_accepts_turso_current_timestamp() -> Result<()> {
    // Turso returns `CURRENT_TIMESTAMP` in the SQLite default shape
    // (`YYYY-MM-DD HH:MM:SS`, no fractional seconds). Our FromSql must
    // accept that without requiring `%.f`.
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(
        "CREATE TABLE events(
            id INTEGER PRIMARY KEY,
            happened_on DATE NOT NULL,
            happened_at_time TIME NOT NULL,
            happened_at TIMESTAMP NOT NULL
        ) STRICT;
         INSERT INTO events VALUES (1, date('2026-04-13'), time('15:30:00'), datetime('2026-04-13 15:30:00'));",
    )
    .await?;
    let got: Event = events::table.find(1i64).first(&mut conn).await?;
    assert_eq!(
        got.happened_on,
        NaiveDate::from_ymd_opt(2026, 4, 13).unwrap()
    );
    assert_eq!(
        got.happened_at_time,
        NaiveTime::from_hms_opt(15, 30, 0).unwrap()
    );
    assert_eq!(
        got.happened_at,
        NaiveDate::from_ymd_opt(2026, 4, 13)
            .unwrap()
            .and_hms_opt(15, 30, 0)
            .unwrap(),
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn datetime_accepts_widened_text_formats() -> Result<()> {
    // We should parse every flavour diesel's SQLite chrono impl accepts:
    // `T` separator, `Z` suffix, explicit `+HH:MM` offsets, minute-only
    // precision, and mixed combinations.
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(PROBES_DDL).await?;

    let expect = NaiveDate::from_ymd_opt(2026, 4, 13)
        .unwrap()
        .and_hms_opt(15, 30, 45)
        .unwrap();
    for (i, lit) in [
        "2026-04-13 15:30:45",
        "2026-04-13T15:30:45",
        "2026-04-13 15:30:45Z",
        "2026-04-13T15:30:45Z",
        "2026-04-13 15:30:45+00:00",
        "2026-04-13T15:30:45+00:00",
    ]
    .into_iter()
    .enumerate()
    {
        let id = i as i64 + 1;
        diesel::insert_into(probes::table)
            .values((probes::id.eq(id), probes::t.eq(lit)))
            .execute(&mut conn)
            .await?;
        let got: NaiveDateTime = probes_as_timestamp::table
            .filter(probes_as_timestamp::id.eq(id))
            .select(probes_as_timestamp::t)
            .get_result(&mut conn)
            .await?;
        assert_eq!(got, expect, "failed to parse {lit:?}");
    }

    // Minute-only precision lands on :00 seconds.
    diesel::insert_into(probes::table)
        .values((probes::id.eq(100i64), probes::t.eq("2026-04-13 15:30")))
        .execute(&mut conn)
        .await?;
    let got: NaiveDateTime = probes_as_timestamp::table
        .filter(probes_as_timestamp::id.eq(100i64))
        .select(probes_as_timestamp::t)
        .get_result(&mut conn)
        .await?;
    assert_eq!(
        got,
        NaiveDate::from_ymd_opt(2026, 4, 13)
            .unwrap()
            .and_hms_opt(15, 30, 0)
            .unwrap()
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn datetime_accepts_julian_day_real() -> Result<()> {
    // turso's `julianday(...)` returns a REAL; our Timestamp FromSql
    // must accept that as a numeric fallback.
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(PROBES_DDL).await?;
    diesel::insert_into(probes::table)
        .values((probes::id.eq(1i64), probes::t.eq("2026-04-13 15:30:45")))
        .execute(&mut conn)
        .await?;

    let got: NaiveDateTime = probes::table
        .select(julianday(probes::t))
        .get_result(&mut conn)
        .await?;
    // Julian-day arithmetic is lossy past seconds; expect a round-trip
    // within a second.
    let expected = NaiveDate::from_ymd_opt(2026, 4, 13)
        .unwrap()
        .and_hms_opt(15, 30, 45)
        .unwrap();
    let diff = (got - expected).num_seconds().abs();
    assert!(
        diff <= 1,
        "julian-day roundtrip off by {diff}s: got {got:?}"
    );
    Ok(())
}

// ----- DateTime<Utc> ↔ Timestamptz ----------------------------------------

diesel::table! {
    use diesel::sql_types::*;
    // Turso's `Timestamptz` is its own SQL type, not the one
    // `postgres_backend` exports, so it has to be named explicitly — the
    // glob above is diesel's backend-agnostic set.
    use diesel::turso::sql_types::Timestamptz;
    tz_events(id) {
        id -> BigInt,
        at -> Timestamptz,
    }
}

#[derive(Insertable, Queryable, Debug, PartialEq)]
#[diesel(table_name = tz_events)]
struct TzEvent {
    id: i64,
    at: DateTime<Utc>,
}

#[tokio::test(flavor = "current_thread")]
async fn timestamptz_roundtrip() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute("CREATE TABLE tz_events(id INTEGER PRIMARY KEY, at TEXT NOT NULL) STRICT")
        .await?;

    let row = TzEvent {
        id: 1,
        at: Utc.with_ymd_and_hms(2026, 4, 13, 15, 30, 45).unwrap(),
    };
    diesel::insert_into(tz_events::table)
        .values(&row)
        .execute(&mut conn)
        .await?;

    let got: TzEvent = tz_events::table.find(1i64).first(&mut conn).await?;
    assert_eq!(got, row);
    Ok(())
}

// ----- chrono types in UNION variants -------------------------------------

#[derive(
    Debug,
    PartialEq,
    Clone,
    FromSqlRow,
    AsExpression,
    diesel::query_builder::QueryId,
    DeriveUnionSchema,
)]
#[diesel(sql_type = TaggedUnion<TimedEvent>)]
pub enum TimedEvent {
    Scheduled(NaiveDateTime),
    Window {
        start: NaiveDateTime,
        end: NaiveDateTime,
        label: String,
    },
    AllDay(NaiveDate),
}

diesel::table! {
    use diesel::sql_types::*;
    use diesel::turso::union::TaggedUnion;
    use super::TimedEvent;
    agenda(id) {
        id -> BigInt,
        v -> TaggedUnion<TimedEvent>,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn datetime_in_union_variants() -> Result<()> {
    let mut conn = TursoConnection::establish(":memory:").await?;
    conn.batch_execute(&TimedEvent::create_type_sql()).await?;
    conn.batch_execute(
        "CREATE TABLE agenda(id INTEGER PRIMARY KEY, v timed_event NOT NULL) STRICT",
    )
    .await?;

    let start = NaiveDate::from_ymd_opt(2026, 4, 13)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    let end = NaiveDate::from_ymd_opt(2026, 4, 13)
        .unwrap()
        .and_hms_opt(11, 30, 0)
        .unwrap();
    let rows = vec![
        (1i64, TimedEvent::Scheduled(start)),
        (
            2,
            TimedEvent::Window {
                start,
                end,
                label: "standup".into(),
            },
        ),
        (
            3,
            TimedEvent::AllDay(NaiveDate::from_ymd_opt(2026, 12, 25).unwrap()),
        ),
    ];
    for (id, v) in rows.iter() {
        diesel::insert_into(agenda::table)
            .values((agenda::id.eq(*id), agenda::v.eq(v.clone())))
            .execute(&mut conn)
            .await?;
    }
    let got: Vec<(i64, TimedEvent)> = agenda::table
        .order(agenda::id.asc())
        .load(&mut conn)
        .await?;
    assert_eq!(got, rows);

    let sql = TimedEvent::create_type_sql();
    assert!(sql.contains("CREATE TYPE window_t AS STRUCT(start TEXT, end TEXT, label TEXT);"));
    assert!(sql.contains(
        "CREATE TYPE timed_event AS UNION(scheduled TEXT, window window_t, all_day TEXT)"
    ));
    Ok(())
}
