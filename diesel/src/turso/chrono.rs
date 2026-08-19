//! `chrono` integration — scalar `ToSql` / `FromSql` impls mapping
//! `crate::sql_types::Date`/`Time`/`Timestamp`/`Timestamptz` to/from
//! `chrono::NaiveDate` / `NaiveTime` / `NaiveDateTime` / `DateTime<Utc>`.
//!
//! Formats are centralised in the `fmt` submodule, which used to matter
//! because a parallel set of `FieldCodec` impls had to stay in sync with
//! these; composites now go through these impls directly, so the module is
//! simply where the formats live. The read lists mirror diesel's SQLite
//! backend so anything the upstream chrono integration accepts also parses
//! here.

use crate::deserialize::{self, FromSql};
use crate::serialize::{self, IsNull, Output, ToSql};
use crate::sql_types;
use ::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};

use crate::turso::backend::Turso;
use crate::turso::value::TursoValue;

pub(crate) mod fmt {
    use ::chrono::{NaiveDate, NaiveDateTime, NaiveTime, ParseResult};

    pub(crate) const DATE: &str = "%F";
    pub(crate) const TIME_WRITE: &str = "%H:%M:%S%.f";
    pub(crate) const DATETIME_WRITE: &str = "%F %T%.f";
    pub(crate) const DATETIMETZ_WRITE: &str = "%F %T%.f%:z";

    pub(crate) const TIME_READ: &[&str] = &[
        "%H:%M:%S%.f",
        "%H:%M:%S",
        "%H:%M",
        "%H:%MZ",
        "%H:%M%:z",
        "%H:%M:%SZ",
        "%H:%M:%S%.fZ",
        "%H:%M:%S%:z",
        "%H:%M:%S%.f%:z",
    ];

    pub(crate) const NAIVE_DATETIME_READ: &[&str] = &[
        "%F %T%.f",
        "%FT%T%.f",
        "%F %T",
        "%FT%T",
        "%F %R",
        "%FT%R",
        "%F %RZ",
        "%FT%RZ",
        "%F %R%:z",
        "%FT%R%:z",
        "%F %TZ",
        "%FT%TZ",
        "%F %T%.fZ",
        "%FT%T%.fZ",
        "%F %T%:z",
        "%FT%T%:z",
        "%F %T%.f%:z",
        "%FT%T%.f%:z",
    ];

    pub(crate) fn parse_date(s: &str) -> ParseResult<NaiveDate> {
        NaiveDate::parse_from_str(s, DATE)
    }

    pub(crate) fn parse_time(s: &str) -> Option<NaiveTime> {
        TIME_READ
            .iter()
            .find_map(|f| NaiveTime::parse_from_str(s, f).ok())
    }

    pub(crate) fn parse_naive_datetime(s: &str) -> Option<NaiveDateTime> {
        NAIVE_DATETIME_READ
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
    }
}

fn expect_text(v: TursoValue<'_>) -> deserialize::Result<&str> {
    match v.as_turso() {
        turso::Value::Text(s) => Ok(s.as_str()),
        other => Err(format!("expected TEXT, got {other:?}").into()),
    }
}

/// Turso emits datetimes as TEXT by default but as REAL / INTEGER when
/// the caller uses `julianday(...)`, so the `Timestamp` and `Timestamptz`
/// FromSql impls both route through this and accept the same set.
pub(crate) fn decode_naive_datetime(v: &turso::Value) -> Option<NaiveDateTime> {
    match v {
        turso::Value::Text(s) => fmt::parse_naive_datetime(s),
        turso::Value::Real(jd) => naive_from_julian_day(*jd),
        turso::Value::Integer(i) => naive_from_julian_day(*i as f64),
        _ => None,
    }
}

/// Julian-day-number → `NaiveDateTime`. JD 2440587.5 is the Unix epoch.
fn naive_from_julian_day(jd: f64) -> Option<NaiveDateTime> {
    let unix_seconds = (jd - 2_440_587.5) * 86_400.0;
    let secs = unix_seconds.trunc() as i64;
    // Carry a sub-ns rounding overflow into the seconds, so values near
    // i64 boundaries don't produce a `nanos == 1e9` that chrono rejects.
    let mut nanos = (unix_seconds.fract() * 1_000_000_000.0).round() as i64;
    let secs = if nanos >= 1_000_000_000 {
        nanos -= 1_000_000_000;
        secs.checked_add(1)?
    } else {
        secs
    };
    DateTime::<Utc>::from_timestamp(secs, nanos as u32).map(|dt| dt.naive_utc())
}

// -- NaiveDate ↔ Date --------------------------------------------------------

impl ToSql<sql_types::Date, Turso> for NaiveDate {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self.format(fmt::DATE).to_string());
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::Date, Turso> for NaiveDate {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        let s = expect_text(v)?;
        fmt::parse_date(s).map_err(|e| format!("parse date {s:?}: {e}").into())
    }
}

// -- NaiveTime ↔ Time --------------------------------------------------------

impl ToSql<sql_types::Time, Turso> for NaiveTime {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self.format(fmt::TIME_WRITE).to_string());
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::Time, Turso> for NaiveTime {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        let s = expect_text(v)?;
        fmt::parse_time(s).ok_or_else(|| format!("parse time {s:?}").into())
    }
}

// -- NaiveDateTime ↔ Timestamp ----------------------------------------------

impl ToSql<sql_types::Timestamp, Turso> for NaiveDateTime {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self.format(fmt::DATETIME_WRITE).to_string());
        Ok(IsNull::No)
    }
}
impl FromSql<sql_types::Timestamp, Turso> for NaiveDateTime {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        let raw = v.as_turso();
        decode_naive_datetime(raw).ok_or_else(|| format!("parse datetime {raw:?}").into())
    }
}

// -- DateTime<Utc> ↔ Timestamptz --------------------------------------------
//
// `crate::turso::sql_types::Timestamptz` comes from diesel's `postgres_backend`
// feature, enabled transitively through our `chrono` feature. Diesel
// owns that SQL type, so its derive-generated `AsExpression` /
// `FromSqlRow` impls on `DateTime<Utc>` don't trip orphan rules the way
// a turbo-diesel-local marker would.
impl ToSql<crate::turso::sql_types::Timestamptz, Turso> for DateTime<Utc> {
    fn to_sql(&self, out: &mut Output<'_, '_, Turso>) -> serialize::Result {
        out.set_value(self.format(fmt::DATETIMETZ_WRITE).to_string());
        Ok(IsNull::No)
    }
}
impl FromSql<crate::turso::sql_types::Timestamptz, Turso> for DateTime<Utc> {
    fn from_sql(v: TursoValue<'_>) -> deserialize::Result<Self> {
        let naive: NaiveDateTime = FromSql::<sql_types::Timestamp, Turso>::from_sql(v)?;
        Ok(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc))
    }
}
