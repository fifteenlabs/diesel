//! `TursoRow` / `TursoField` — diesel `Row` and `Field` impls over a
//! buffered row from `turso::Connection::query`.

use std::sync::Arc;

use crate::backend::Backend;
use crate::row::{Field, PartialRow, Row, RowIndex, RowSealed};

use crate::turso::backend::Turso;
use crate::turso::value::TursoValue;

/// One row of a result set, buffered. Column names are shared across every
/// row of a given result set via `Arc` so we don't clone a `Vec<String>`
/// per row.
#[derive(Debug)]
pub struct TursoRow<'a> {
    values: Vec<turso::Value>,
    column_names: Arc<[String]>,
    _lt: std::marker::PhantomData<&'a ()>,
}

impl<'a> TursoRow<'a> {
    /// Buffer one row of a result set.
    ///
    /// `pub` rather than `pub(crate)` because the only caller is the
    /// connection, and the connection is in `diesel-async` (see the module
    /// docs on [`crate::turso`]).
    pub fn new(values: Vec<turso::Value>, column_names: Arc<[String]>) -> Self {
        Self {
            values,
            column_names,
            _lt: std::marker::PhantomData,
        }
    }
}

impl<'a> RowSealed for TursoRow<'a> {}

impl<'a> Row<'a, Turso> for TursoRow<'a> {
    type Field<'f>
        = TursoField<'f, 'a>
    where
        Self: 'f;
    type InnerPartialRow = Self;

    fn field_count(&self) -> usize {
        self.values.len()
    }

    fn get<'b, I>(&'b self, idx: I) -> Option<Self::Field<'b>>
    where
        'a: 'b,
        Self: RowIndex<I>,
    {
        let col = self.idx(idx)?;
        Some(TursoField { row: self, col })
    }

    fn partial_row(&self, range: std::ops::Range<usize>) -> PartialRow<'_, Self::InnerPartialRow> {
        PartialRow::new(self, range)
    }
}

impl<'a> RowIndex<usize> for TursoRow<'a> {
    fn idx(&self, idx: usize) -> Option<usize> {
        (idx < self.values.len()).then_some(idx)
    }
}

impl<'a, 'n> RowIndex<&'n str> for TursoRow<'a> {
    fn idx(&self, name: &'n str) -> Option<usize> {
        self.column_names.iter().position(|c| c == name)
    }
}

/// One cell of a [`TursoRow`], addressed by column position.
#[derive(Debug)]
pub struct TursoField<'f, 'row> {
    row: &'f TursoRow<'row>,
    col: usize,
}

impl<'f, 'row> Field<'f, Turso> for TursoField<'f, 'row> {
    fn field_name(&self) -> Option<&str> {
        self.row.column_names.get(self.col).map(String::as_str)
    }

    fn value(&self) -> Option<<Turso as Backend>::RawValue<'_>> {
        self.row
            .values
            .get(self.col)
            .filter(|v| !matches!(v, turso::Value::Null))
            .map(TursoValue::new)
    }
}
