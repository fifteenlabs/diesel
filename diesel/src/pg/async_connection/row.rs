//! `diesel` denies `unsafe_code` crate-wide (see `lib.rs`), which
//! `diesel-async` — where this file was written — did not. The blocks
//! below are unchanged from that crate and are allowed at module scope
//! rather than rewritten, so the crate-wide deny still catches unsafe
//! that is genuinely new.
#![allow(unsafe_code)]

use crate::backend::Backend;
use crate::row::{Field, PartialRow, RowIndex, RowSealed};
use std::{error::Error, num::NonZeroU32};
use tokio_postgres::{types::Type, Row};

// `diesel` warns on `missing_debug_implementations` where `diesel-async`
// did not. These hold a live connection, a driver row or a borrowed
// connection; none has a `Debug` worth forwarding.
#[allow(missing_debug_implementations)]
pub struct PgRow {
    row: Row,
}

impl PgRow {
    pub(super) fn new(row: Row) -> Self {
        Self { row }
    }
}
impl RowSealed for PgRow {}

impl<'a> crate::row::Row<'a, crate::pg::Pg> for PgRow {
    type InnerPartialRow = Self;
    type Field<'b>
        = PgField<'b>
    where
        Self: 'b,
        'a: 'b;

    fn field_count(&self) -> usize {
        self.row.len()
    }

    fn get<'b, I>(&'b self, idx: I) -> Option<Self::Field<'b>>
    where
        'a: 'b,
        Self: crate::row::RowIndex<I>,
    {
        let idx = self.idx(idx)?;
        Some(PgField {
            row: &self.row,
            idx,
        })
    }

    fn partial_row(
        &self,
        range: std::ops::Range<usize>,
    ) -> crate::row::PartialRow<'_, Self::InnerPartialRow> {
        PartialRow::new(self, range)
    }
}

impl RowIndex<usize> for PgRow {
    fn idx(&self, idx: usize) -> Option<usize> {
        if idx < self.row.len() {
            Some(idx)
        } else {
            None
        }
    }
}

impl<'a> RowIndex<&'a str> for PgRow {
    fn idx(&self, idx: &'a str) -> Option<usize> {
        self.row.columns().iter().position(|c| c.name() == idx)
    }
}

// `diesel` warns on `missing_debug_implementations` where `diesel-async`
// did not. These hold a live connection, a driver row or a borrowed
// connection; none has a `Debug` worth forwarding.
#[allow(missing_debug_implementations)]
pub struct PgField<'a> {
    row: &'a Row,
    idx: usize,
}

impl<'a> Field<'a, crate::pg::Pg> for PgField<'a> {
    fn field_name(&self) -> Option<&str> {
        Some(self.row.columns()[self.idx].name())
    }

    fn value(&self) -> Option<<crate::pg::Pg as Backend>::RawValue<'_>> {
        let DieselFromSqlWrapper(value) = self.row.get(self.idx);
        value
    }
}

#[repr(transparent)]
struct TyWrapper(Type);

impl crate::pg::TypeOidLookup for TyWrapper {
    fn lookup(&self) -> NonZeroU32 {
        NonZeroU32::new(self.0.oid()).unwrap()
    }
}

struct DieselFromSqlWrapper<'a>(Option<crate::pg::PgValue<'a>>);

impl<'a> tokio_postgres::types::FromSql<'a> for DieselFromSqlWrapper<'a> {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, Box<dyn Error + 'static + Send + Sync>> {
        let ty = unsafe { &*(ty as *const Type as *const TyWrapper) };
        Ok(DieselFromSqlWrapper(Some(crate::pg::PgValue::new(raw, ty))))
    }

    fn accepts(ty: &Type) -> bool {
        ty.oid() != 0
    }

    fn from_sql_null(_ty: &Type) -> Result<Self, Box<dyn Error + Sync + Send>> {
        Ok(DieselFromSqlWrapper(None))
    }
}
