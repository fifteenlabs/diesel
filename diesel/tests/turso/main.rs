//! The Turso backend's acceptance suite.
//!
//! One test binary rather than thirty-odd: each of these opens its own
//! temporary database, so they cost nothing to run together, and one
//! binary links the (large) query-builder generics once instead of
//! once per file.

mod batch_insert;
mod case_expression;
mod chrono_support;
mod compiles_m1;
mod database_errors;
mod datetime_storage;
mod datetime_support;
mod deserialization_errors;
mod expr_functions;
mod joins;
mod limit_offset;
mod m3_establish_exec;
mod m4_load;
mod m5_scalars;
mod m6_metadb_non_union;
mod m7_union_codec;
mod m8_union_derive;
mod m9_migrations;
mod nested_transactions;
mod pragma;
mod query_plans;
mod replace_into;
mod returning_clause;
mod scalar_union;
mod shared_string;
mod statement_cache;
mod stream_cancellation;
mod strict_type_support;
mod subselect_limit;
mod to_sql_owned;
mod type_probe;
mod union_boxed;
mod union_expressions;
mod upsert;
mod wire_differential;
