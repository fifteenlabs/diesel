//! Reaching inside a UNION column from the DSL: `union_extract`,
//! `struct_extract` and `union_tag` as typed expression nodes.
//!
//! A UNION column crosses the wire as one opaque blob (see [`codec`]), so
//! `table!` has nothing to name below the column itself, and until now the
//! only way to ask about a variant or a field was a `dsl::sql` fragment:
//!
//! ```ignore
//! sql::<Bool>("struct_extract(union_extract(mid, 'telegram'), 'chat_id') = ")
//!     .bind::<BigInt, _>(chat_id)
//! ```
//!
//! Three things are wrong with that, and this module fixes all three. The
//! field name is a string, so a rename is a runtime `SqliteFailure`. The
//! bind type is asserted rather than checked, so `BigInt` against a TEXT
//! field is a silent Turso coercion. And a `SqlLiteral` reports itself
//! unsafe to cache, which takes the *whole enclosing statement* out of the
//! statement cache — the fragment is three functions wide but the cost is
//! the size of the query around it.
//!
//! The typed form is the same shape and none of that:
//!
//! ```ignore
//! use fifteen_db::schema::meta::{message_id::telegram, messages};
//!
//! messages::table
//!     .filter(messages::mid.extract(telegram::variant).is_not_null())
//!     .filter(messages::mid.extract(telegram::variant).field(telegram::chat_id).eq(chat_id))
//! ```
//!
//! `telegram::variant` and `telegram::chat_id` are ZSTs `#[derive(UnionSchema)]`
//! emits beside the enum, the way `table!` emits a type per column — so the
//! field name is a path the compiler resolves, its SQL type comes from the
//! enum's own field type, and the nodes carry a real [`QueryId`].
//!
//! # Everything here is nullable
//!
//! `union_extract` returns NULL when the value's tag isn't the one asked
//! for, so [`Extract`] is nullable and so is every [`GetField`] under it.
//! That is the feature, not a wart: `.is_not_null()` *is* the tag test, and
//! diesel makes callers face the mismatch case instead of assuming it away.
//! Where a caller has already filtered to the variant, diesel's own
//! `.assume_not_null()` says so in one call.
//!
//! # Names are SQL text, not binds
//!
//! Turso resolves a union tag to its declaration ordinal while translating
//! the statement and rejects a bind there, so [`Extract`] pushes the tag as
//! literal SQL. [`GetField`] does the same for the field name, for symmetry
//! and because a bind would defeat the expression indexes. Neither string
//! ever comes from a caller: both are consts on derive-generated types.
//!
//! # A name is not always stored as it was written
//!
//! The consts those nodes push are already in Turso's spelling, not the
//! Rust one, and the difference is not cosmetic. Turso re-renders a
//! `CREATE TYPE` from its own AST before persisting it and quotes any
//! member name that would not lex back as a plain identifier — so
//! `UNION(first INT, second INT)` is stored as `UNION("first" INT, second
//! INT)`, and the two quote characters become *part of the variant's name*.
//! `union_tag(col)` then returns the seven-character string `"first"`, and
//! `union_extract(col, 'first')` is not a NULL but
//! `Parse error: cannot resolve union variant 'first'`.
//!
//! Nothing on the whole-value path notices, because `ToSql` binds the wire
//! blob and never names a tag — such a union inserts, selects and
//! round-trips perfectly and then fails at the first tag test. So
//! `#[derive(UnionSchema)]` computes each name's stored spelling at
//! expansion time (its `stored_name`, which asks Turso's own lexer whether
//! the name is a keyword) and emits *that* as [`UnionVariant::TAG_NAME`],
//! [`CompositeField::NAME`] and the DDL member name alike. One spelling
//! everywhere, so `union_tag(col).eq(TAG_NAME)` and
//! `col.extract(v)` agree with the database and with each other, and the
//! nodes below can push a const with no per-query work.
//!
//! For every name that needs no quoting — which is every name in this
//! workspace — the stored spelling and the written one are the same string
//! and nothing renders differently than it did.
//!
//! # Indexes still match
//!
//! `messages_tg` and friends are expression indexes over
//! `struct_extract(union_extract(mid, 'telegram'), 'chat_id')`, written
//! unqualified, while diesel renders the column as `"messages"."mid"` and
//! parenthesises comparisons. Turso matches an expression index by
//! comparing resolved expression trees, not text, so the qualification and
//! the parentheses make no difference — `fifteen-db`'s
//! `tests/expression_index_plans.rs` pins that with `EXPLAIN QUERY PLAN`
//! against the real indexes, one assertion per index.
//!
//! [`codec`]: super::codec

use std::marker::PhantomData;

use crate::expression::{AppearsOnTable, Expression, SelectableExpression, TypedExpressionType};
use crate::query_builder::{AstPass, QueryFragment, QueryId};
// `SqlType` is imported for its *derive*, which is what gives `Composite`
// `SingleValue` and hence a `TypedExpressionType`.
use crate::sql_types::{HasSqlType, Nullable, SqlType};

use crate::turso::backend::{Turso, TursoType};
use crate::turso::union::TaggedUnion;

// ── the types the derive generates ───────────────────────────────────────

/// One variant of a UNION type, as a type: `message_id::telegram::variant`.
///
/// Generated by `#[derive(UnionSchema)]`, one per variant, alongside a ZST
/// per field of a struct variant.
pub trait UnionVariant: 'static {
    /// The Rust enum this is a variant of. What ties a variant ZST to the
    /// columns it may be applied to.
    type Union: 'static;

    /// The SQL type `union_extract(col, TAG_NAME)` yields — always
    /// nullable, because a tag mismatch is a NULL. `Nullable<Composite<F>>`
    /// for a struct variant; the scalar's own nullable SQL type otherwise,
    /// which is what makes `.field(…)` a compile error on a scalar variant.
    type Payload;

    /// Declaration ordinal, i.e. the wire tag byte. Not used in SQL —
    /// Turso wants the name — but it is the variant's real identity, and
    /// carrying it here keeps one source of truth with the codec.
    const TAG: u8;

    /// The name in `CREATE TYPE … AS UNION(…)`, the literal [`Extract`]
    /// pushes, and the string `union_tag` returns — one spelling, because
    /// Turso has one.
    ///
    /// **In Turso's spelling, not the Rust one.** A tag the engine requotes
    /// on the way into the schema is *named* with the quotes afterwards, so
    /// a keyword tag's `TAG_NAME` is `"first"` — seven characters — and
    /// that is what `union_extract` resolves and what `union_tag` compares
    /// equal to. See the module docs; a hand-written impl has to apply the
    /// same rule, and the derive applies it for you.
    const TAG_NAME: &'static str;
}

/// The field set of one STRUCT payload, as a type:
/// `message_id::telegram::fields`. Fields are typed against it, so
/// `slack::channel_id` cannot be projected out of a `telegram` extract.
pub trait CompositeShape: 'static {
    /// Declaration-order field names. Rendering and error messages only;
    /// the DSL addresses a field by its type.
    const FIELD_NAMES: &'static [&'static str];
}

/// One field of one composite: `message_id::telegram::chat_id`.
///
/// The composite is an associated type rather than a parameter because a
/// field ZST belongs to exactly one field set — `telegram::chat_id` and
/// `self_chat::user_id` are different types even where the names collide —
/// and because the rendering half needs the field's name without knowing
/// which shape it was reached through.
pub trait CompositeField: 'static {
    /// The field set this belongs to, matched against the shape of the
    /// expression `.field(…)` is called on.
    type Shape;

    /// What `struct_extract` yields for this field — the field's own SQL
    /// type made nullable, since the `union_extract` under it may have
    /// missed. `.assume_not_null()` is how a caller that has already
    /// filtered on the variant gets back to the plain type.
    type SqlType;

    /// The name in `CREATE TYPE … AS STRUCT(…)`, and the literal
    /// [`GetField`] pushes — in Turso's spelling, quotes and all where the
    /// engine requotes it. Same rule and same reason as
    /// [`UnionVariant::TAG_NAME`].
    const NAME: &'static str;

    /// Position in the STRUCT's declaration order — the same index the
    /// wire codec writes this field at, so the DSL and the codec cannot
    /// disagree about which field is which.
    const INDEX: usize;
}

/// SQL type of a STRUCT payload: what `union_extract` gives back for a
/// struct variant, and the only thing [`GetField`] projects out of.
///
/// One flat row field like every other scalar SQL type — `#[derive(SqlType)]`
/// gives it `SingleValue`, so nothing in diesel's row slicing has to know
/// composites exist.
#[derive(SqlType, QueryId, Debug, Clone, Copy, Default)]
pub struct Composite<S: 'static>(PhantomData<fn() -> S>);

impl<S: 'static> HasSqlType<Composite<S>> for Turso {
    fn metadata(_: &mut ()) -> TursoType {
        TursoType::Binary
    }
}

// ── which SQL types these nodes apply to ─────────────────────────────────

/// An expression `union_extract` / `union_tag` may be applied to: a UNION
/// column, or a nullable one.
pub trait UnionSqlType {
    /// The Rust enum behind the column.
    type Union;
}

impl<E: 'static> UnionSqlType for TaggedUnion<E> {
    type Union = E;
}

impl<E: 'static> UnionSqlType for Nullable<TaggedUnion<E>> {
    type Union = E;
}

/// An expression `struct_extract` may be applied to — always the result of
/// an [`Extract`] on a struct variant, since nothing else has this type.
pub trait CompositeSqlType {
    /// The field-set marker, which is what a [`CompositeField`] is typed
    /// against.
    type Shape;
}

impl<S: 'static> CompositeSqlType for Composite<S> {
    type Shape = S;
}

impl<S: 'static> CompositeSqlType for Nullable<Composite<S>> {
    type Shape = S;
}

// ── the nodes ────────────────────────────────────────────────────────────

/// `union_extract(<expr>, '<tag>')` — the payload of one variant, or NULL
/// if the value is some other variant. Built by
/// [`UnionExpressionMethods::extract`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Extract<E, V> {
    expr: E,
    variant: PhantomData<fn() -> V>,
}

/// `struct_extract(<expr>, '<field>')` — one field of a composite. Built by
/// [`CompositeExpressionMethods::field`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GetField<E, F> {
    expr: E,
    field: PhantomData<fn() -> F>,
}

/// `union_tag(<expr>)` — the tag name a UNION value carries, as TEXT. Built
/// by [`UnionExpressionMethods::union_tag`].
///
/// [`Extract`] plus `.is_not_null()` answers the same question for a known
/// variant and is the form that reads better in a filter; this one exists
/// for the cases that want the tag as a value — a `select`, an `order`, or
/// a comparison written to mirror SQL that already says `union_tag`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnionTag<E> {
    expr: E,
}

// ── typing ───────────────────────────────────────────────────────────────

impl<E, V> Expression for Extract<E, V>
where
    E: Expression,
    E::SqlType: UnionSqlType<Union = V::Union>,
    V: UnionVariant,
    V::Payload: TypedExpressionType,
{
    type SqlType = V::Payload;
}

impl<E, F> Expression for GetField<E, F>
where
    E: Expression,
    E::SqlType: CompositeSqlType,
    F: CompositeField<Shape = <E::SqlType as CompositeSqlType>::Shape>,
    F::SqlType: TypedExpressionType,
{
    type SqlType = F::SqlType;
}

impl<E> Expression for UnionTag<E>
where
    E: Expression,
    E::SqlType: UnionSqlType,
{
    // Never NULL: every stored UNION value has a tag. A nullable column
    // could still be NULL as a whole, but that is the column's business and
    // `Nullable<Text>` here would push it onto every caller.
    type SqlType = crate::sql_types::Text;
}

// `QS` — the query source the expression is used against — is propagated
// rather than blanket-implemented, exactly as `table!` does it for a column
// (`diesel_derives/src/table.rs`): `messages::mid.extract(…)` appears on
// precisely the tables `messages::mid` appears on, and on no others.
impl<QS, E, V> AppearsOnTable<QS> for Extract<E, V>
where
    E: AppearsOnTable<QS>,
    Self: Expression,
{
}

impl<QS, E, F> AppearsOnTable<QS> for GetField<E, F>
where
    E: AppearsOnTable<QS>,
    Self: Expression,
{
}

impl<QS, E> AppearsOnTable<QS> for UnionTag<E>
where
    E: AppearsOnTable<QS>,
    Self: Expression,
{
}

impl<QS, E, V> SelectableExpression<QS> for Extract<E, V>
where
    E: SelectableExpression<QS>,
    Self: AppearsOnTable<QS>,
{
}

impl<QS, E, F> SelectableExpression<QS> for GetField<E, F>
where
    E: SelectableExpression<QS>,
    Self: AppearsOnTable<QS>,
{
}

impl<QS, E> SelectableExpression<QS> for UnionTag<E>
where
    E: SelectableExpression<QS>,
    Self: AppearsOnTable<QS>,
{
}

// Aggregate-ness is the operand's: `union_extract` is a scalar function, so
// it neither introduces nor swallows an aggregate. Delegating is what lets
// `COUNT(union_extract(…))` and a bare projection both type-check.
impl<GB, E, V> crate::expression::ValidGrouping<GB> for Extract<E, V>
where
    E: crate::expression::ValidGrouping<GB>,
{
    type IsAggregate = E::IsAggregate;
}

impl<GB, E, F> crate::expression::ValidGrouping<GB> for GetField<E, F>
where
    E: crate::expression::ValidGrouping<GB>,
{
    type IsAggregate = E::IsAggregate;
}

impl<GB, E> crate::expression::ValidGrouping<GB> for UnionTag<E>
where
    E: crate::expression::ValidGrouping<GB>,
{
    type IsAggregate = E::IsAggregate;
}

// Which expressions a `GROUP BY` over one of these nodes makes selectable.
//
// `ValidGrouping` above answers "may this expression appear alongside
// aggregates", and for a column it answers it by asking
// `IsContainedInGroupBy`: `table!` writes `IsContainedInGroupBy<col> for
// col`, so `GROUP BY col` makes `col` — and, through the delegation above,
// every expression over `col` — selectable. Nothing wrote the mirror
// image, so `GROUP BY union_tag(col)` could not select the tag it grouped
// by: `col: ValidGrouping<UnionTag<col>>` wants `UnionTag<col>:
// IsContainedInGroupBy<col>`, and there was no such impl. That was a gap
// in these nodes rather than a fact about SQL — Turso runs the query — and
// it made the natural `.select((col.union_tag(), count_star()))` a compile
// error while the same query written with `count_star()` alone was fine.
//
// So each node forwards the question to its operand, which is the only
// shape available: the verdict for `UnionTag<col>` has to travel through
// `col`'s own `ValidGrouping` impl, so it cannot be made narrower than
// `col`'s. The consequence is that grouping by one of these expressions
// also admits the operand and its siblings — `GROUP BY union_tag(col)`
// will let you select a bare `col`, which SQLite and Turso allow (an
// arbitrary row from the group) but which is not functionally determined.
// Diesel's model has one bit here and this is the bit that keeps the
// determined case working; the alternative would be a second
// `ValidGrouping` impl per node, overlapping the delegating one.
//
// All three nodes get it, because all three are pure scalar functions of
// their operand: `union_tag(col)`, `union_extract(col, 't')` and
// `struct_extract(union_extract(col, 't'), 'f')` are each constant within
// a group of equal operands, so if the operand is grouped, so are they.
// Giving it to only one would leave `GROUP BY union_extract(…)` — a real
// query, since a struct variant's payload is what one groups by — with
// exactly the defect this removes.

impl<E, T, V> crate::expression::IsContainedInGroupBy<T> for Extract<E, V>
where
    E: crate::expression::IsContainedInGroupBy<T>,
{
    type Output = E::Output;
}

impl<E, T, F> crate::expression::IsContainedInGroupBy<T> for GetField<E, F>
where
    E: crate::expression::IsContainedInGroupBy<T>,
{
    type Output = E::Output;
}

impl<E, T> crate::expression::IsContainedInGroupBy<T> for UnionTag<E>
where
    E: crate::expression::IsContainedInGroupBy<T>,
{
    type Output = E::Output;
}

// `QueryId` decides whether the enclosing statement can be prepared once
// and reused. These nodes are the whole reason this module exists — a
// `dsl::sql` fragment reports itself unsafe to cache and takes the
// statement with it — so it is propagated, never opted out of.
impl<E: QueryId, V: 'static> QueryId for Extract<E, V> {
    type QueryId = Extract<E::QueryId, V>;
    const HAS_STATIC_QUERY_ID: bool = E::HAS_STATIC_QUERY_ID;
}

impl<E: QueryId, F: 'static> QueryId for GetField<E, F> {
    type QueryId = GetField<E::QueryId, F>;
    const HAS_STATIC_QUERY_ID: bool = E::HAS_STATIC_QUERY_ID;
}

impl<E: QueryId> QueryId for UnionTag<E> {
    type QueryId = UnionTag<E::QueryId>;
    const HAS_STATIC_QUERY_ID: bool = E::HAS_STATIC_QUERY_ID;
}

// ── rendering ────────────────────────────────────────────────────────────

impl<E, V> QueryFragment<Turso> for Extract<E, V>
where
    E: QueryFragment<Turso>,
    V: UnionVariant,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        out.push_sql("union_extract(");
        self.expr.walk_ast(out.reborrow())?;
        out.push_sql(", '");
        // Not a bind: Turso resolves the tag to an ordinal while
        // translating and rejects a parameter in that position. Safe —
        // `TAG_NAME` is a const on a derive-generated type, never input.
        //
        // Pushed verbatim because it is already Turso's spelling of the
        // name: for a tag the engine requotes when it stores the
        // declaration, `TAG_NAME` carries the quotes, and what lands here
        // is `union_extract(col, '"first"')` — the only form that resolves.
        // See the module docs for what happens without that.
        out.push_sql(V::TAG_NAME);
        out.push_sql("')");
        Ok(())
    }
}

impl<E, F> QueryFragment<Turso> for GetField<E, F>
where
    E: QueryFragment<Turso>,
    F: CompositeField,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        out.push_sql("struct_extract(");
        self.expr.walk_ast(out.reborrow())?;
        out.push_sql(", '");
        // Turso's spelling of the field name, for the same reason as
        // `Extract` above.
        out.push_sql(F::NAME);
        out.push_sql("')");
        Ok(())
    }
}

impl<E> QueryFragment<Turso> for UnionTag<E>
where
    E: QueryFragment<Turso>,
{
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Turso>) -> crate::QueryResult<()> {
        out.push_sql("union_tag(");
        self.expr.walk_ast(out.reborrow())?;
        out.push_sql(")");
        Ok(())
    }
}

// ── the DSL ──────────────────────────────────────────────────────────────

/// `.extract(variant)` and `.union_tag()` on any UNION-typed expression.
///
/// Blanket-implemented like diesel's own `ExpressionMethods`: the bounds
/// that matter live on the methods, so the trait can be imported once and
/// the column types decide what is legal.
pub trait UnionExpressionMethods: Expression + Sized {
    /// `union_extract(self, '<tag>')` — the variant's payload, NULL if the
    /// row holds a different variant.
    fn extract<V>(self, _variant: V) -> Extract<Self, V>
    where
        V: UnionVariant,
        Self::SqlType: UnionSqlType<Union = V::Union>,
    {
        Extract {
            expr: self,
            variant: PhantomData,
        }
    }

    /// `union_tag(self)` — which variant this value is, as TEXT.
    fn union_tag(self) -> UnionTag<Self>
    where
        Self::SqlType: UnionSqlType,
    {
        UnionTag { expr: self }
    }
}

impl<T: Expression> UnionExpressionMethods for T {}

/// `.field(field)` on a STRUCT payload — i.e. on the result of
/// [`extract`](UnionExpressionMethods::extract).
pub trait CompositeExpressionMethods: Expression + Sized {
    /// `struct_extract(self, '<field>')`.
    fn field<F>(self, _field: F) -> GetField<Self, F>
    where
        Self::SqlType: CompositeSqlType,
        F: CompositeField<Shape = <Self::SqlType as CompositeSqlType>::Shape>,
    {
        GetField {
            expr: self,
            field: PhantomData,
        }
    }
}

impl<T: Expression> CompositeExpressionMethods for T {}

/// The nullable form of a SQL type, spelled once so the derive can write it
/// without caring whether the field was already `Option<_>`.
///
/// `IntoNullable` is diesel's; this alias is only shorter at the ~200 call
/// sites the derive generates.
pub type NullableOf<ST> = <ST as crate::sql_types::IntoNullable>::Nullable;

/// `Nullable<Composite<S>>` — a struct variant's extracted payload type,
/// spelled for the derive.
pub type NullableComposite<S> = Nullable<Composite<S>>;
