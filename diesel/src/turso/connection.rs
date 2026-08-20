//! `TursoConnection` — `crate::connection::AsyncConnection<Backend = Turso>`.
//!
//! Wraps a `turso::Connection` plus the diesel transaction-manager state.
//! `establish`, `batch_execute`, `execute_returning_count`, and `load` all
//! go through a single `prepare` helper that serialises the SQL + collects
//! bound parameters.
//!
//! Serialised SQL is then compiled by Turso, and repeated SQL is compiled
//! from its cache rather than parsed again — see [`StatementCache`].

// Trait signatures use `-> impl Future` rather than `async fn`; we mirror
// them verbatim.
#![allow(clippy::manual_async_fn)]

use std::collections::VecDeque;
use std::future::Future;
use std::hash::{DefaultHasher, Hash, Hasher};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::pin::Pin;
use std::sync::Arc;

use crate::connection::AnsiAsyncTransactionManager;
use crate::connection::{AsyncConnection, AsyncConnectionCore, SimpleAsyncConnection};
use crate::connection::{CacheSize, Instrumentation};
use crate::query_builder::{AsQuery, QueryBuilder, QueryFragment, QueryId};
use crate::{ConnectionResult, QueryResult};
use futures_util::stream::{self, BoxStream};

use crate::turso::bind::TursoBindCollector;
use crate::turso::error::{turso_to_connection, turso_to_diesel};
use crate::turso::row::TursoRow;
use crate::turso::{Turso, TursoQueryBuilder};

/// Distinct statements one connection will admit into Turso's cache.
///
/// The cache itself is Turso's — a map from SQL text to the compiled program,
/// which it never evicts from — so the bound has to be ours: past this many,
/// further SQL keeps going through a one-shot `prepare`. Diesel emits `?`
/// placeholders for values, so what this counts is query *shapes* rather than
/// queries, and a few hundred covers the app several times over. It is a
/// backstop under [`StatementCache::admit`]'s real filter rather than the
/// thing doing the work — and because it is the backstop, reaching it is
/// reported rather than absorbed. See [`StatementCacheStats::refused_at_cap`].
const MAX_CACHED_STATEMENTS: usize = 512;

/// Statement families held on probation — texts seen once, waiting to be seen
/// again — and families known to be variadic.
///
/// Bounded and FIFO, because the population it holds is not the app's query
/// set: a statement built with `format!` mints a new family every call, and
/// each one lands here. Forgetting the oldest costs nothing (a statement that
/// really does recur will be back long before 512 other *distinct* texts have
/// gone past), whereas an unbounded map would turn a `format!` site into a
/// slow leak.
const MAX_PROBATION_FAMILIES: usize = 512;

/// What a run of placeholders collapses to in a family key. Any byte that
/// cannot appear in SQL will do; the point is only that two texts differing
/// solely in how many placeholders sit in one run hash the same.
const PLACEHOLDER_RUN: u8 = 0xff;

/// Bytes a variadic placeholder run is made of. `IN (?)`, `IN (?, ?, ?)` and
/// `VALUES (?, ?), (?, ?)` are all nothing but placeholders, commas,
/// parentheses and whitespace. Anything else — a column name, an operator, a
/// keyword — ends the run, which is what keeps `SET "a" = ?, "b" = ?` (two
/// independent slots, with `"b" =` between them) from reading as one.
const fn is_run_byte(b: u8) -> bool {
    matches!(b, b'?' | b',' | b'(' | b')' | b' ' | b'\t' | b'\n' | b'\r')
}

/// The key a statement is remembered by.
///
/// FxHash rather than the default SipHash, because this runs on *every*
/// statement execution — [`StatementCache::admit`] calls it before anything
/// else — and a collision here is harmless by construction: Turso keys its
/// own cache by the SQL text, so a wrong answer can only change whether a
/// statement is cached, never which program runs. That is the exact trade a
/// fast, weak hasher is for.
///
/// Measured on a 355-byte chat-page `SELECT`, release build: 84 ns with
/// SipHash against 12 ns with FxHash, plus 6 ns against 1 ns for the
/// [`FxHashSet`] probe that follows it. So about 77 ns per execution, which
/// is real and which is also not going to show up anywhere: against the
/// 130 µs a paged read of a large thread costs it is 0.06%. It is done
/// because it is free, not because it was a bottleneck.
fn hash_of(sql: &str) -> u64 {
    let mut hasher = FxHasher::default();
    sql.hash(&mut hasher);
    hasher.finish()
}

/// The *family* a statement belongs to: its SQL text with every maximal span
/// of [run bytes](is_run_byte) that contains a placeholder collapsed to one
/// token.
///
/// Two texts share a family exactly when they are identical outside those
/// spans — which, since a span is by construction all punctuation, means
/// exactly when they differ in how many placeholders sit in one slot. That is
/// to say: when one is a longer `IN (?, ?, …)` list or a taller multi-row
/// `VALUES (?, ?), (?, ?)` than the other. A family that has produced two
/// texts is therefore a variadic call site, caught by direct evidence rather
/// than by guessing from the text of any one statement in isolation.
///
/// The span has to swallow a *single* placeholder too, not just runs of two
/// or more: `IN (?)` is the one-element case of the same `eq_any`, and if it
/// keyed differently from `IN (?, ?)` the call site would never be seen to
/// produce two texts under one key.
///
/// Punctuation with no placeholder in it — the `WHERE (` of a filter, the
/// spaces between keywords — is hashed as it stands, so two statements that
/// differ in structure still differ here.
///
/// Hashed as it is computed rather than built into a `String` first, because
/// this runs on every write the app makes.
///
/// # Why this one keeps SipHash while [`hash_of`] does not
///
/// It looks like an inconsistency and it is not: it was measured, and FxHash
/// loses here. This hasher is fed one byte at a time — that is forced by the
/// canonicalisation, which has to decide per byte whether it is inside a
/// placeholder run — and FxHash pays a multiply-and-rotate on every `write_u8`
/// where SipHash accumulates into a word first. On the same 355-byte
/// statement: 203 ns for SipHash against 351 ns for FxHash, i.e. 1.7× slower.
///
/// Rewriting the loop to hash whole spans with `write(&bytes[a..b])` does not
/// rescue it either — 216 ns with FxHash, still behind the 203 ns this costs
/// now, because at that point the scan dominates and the extra branching to
/// find span boundaries costs more than the hashing saved.
///
/// So: do not "fix" the inconsistency without re-running the numbers. The two
/// functions are hashing in different shapes and the right hasher differs.
fn family_of(sql: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !is_run_byte(bytes[i]) {
            hasher.write_u8(bytes[i]);
            i += 1;
            continue;
        }
        let start = i;
        let mut has_placeholder = false;
        while i < bytes.len() && is_run_byte(bytes[i]) {
            has_placeholder |= bytes[i] == b'?';
            i += 1;
        }
        if has_placeholder {
            hasher.write_u8(PLACEHOLDER_RUN);
        } else {
            for byte in &bytes[start..i] {
                hasher.write_u8(*byte);
            }
        }
    }
    hasher.finish()
}

/// What a statement family is known to be. Absent from the map means "never
/// seen"; present means one of these.
#[derive(Clone, Copy)]
enum Family {
    /// One text so far. Seeing *that same text* again is the recurrence that
    /// admits it — and the entry stays afterwards, because it is what a
    /// second, different text has to contradict. Dropping it on admission
    /// would let an `eq_any` whose first list length happened to repeat
    /// admit a second length the same way, and a third, one slot each.
    Single(u64),
    /// Two texts have come out of this family, so its text is a function of
    /// runtime data — an `eq_any` list length, a multi-row insert's row
    /// count. Nothing from it is ever admitted again.
    ///
    /// Reaching this state can cost one slot: if the first list length seen
    /// recurs before any other length shows up, it is admitted, and the
    /// second length is what closes the family. That is the price of deciding
    /// from evidence instead of from a pattern match on a single text — and
    /// it is a price worth paying, because the alternative filter that would
    /// have caught it (a text containing a run of adjacent placeholders is
    /// variadic) also catches every upsert, whose `VALUES (?, ?)` is a run of
    /// exactly the same shape. One slot per variadic call site is bounded by
    /// the number of call sites, holds a statement that is perfectly good to
    /// cache anyway, and never grows with list length, which is the thing
    /// that had to be prevented.
    Variadic,
}

/// What the statement cache has been doing, for tests and the perf report.
///
/// Counts executions, not statements, except for [`Self::admitted`] — the
/// question a caller asks of this is "did that query take the cached path?",
/// and the answer has to be countable, because the failure mode this whole
/// mechanism has is silent: an uncached statement returns exactly the right
/// rows, just after recompiling the program to get them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatementCacheStats {
    /// Distinct SQL texts compiled through Turso's cache.
    pub admitted: usize,
    /// Executions served by an already-admitted text — the cache working.
    pub hits: u64,
    /// Executions compiled one-shot: refused, on probation, or variadic.
    pub uncached: u64,
    /// Executions that would have been admitted but for
    /// [`MAX_CACHED_STATEMENTS`]. Non-zero means the app is paying the
    /// compile cost this cache exists to remove, and nothing else would say
    /// so — the query still returns the right answer.
    pub refused_at_cap: u64,
    /// Families found to generate more than one text. Roughly "variadic call
    /// sites reached so far"; a number that climbs without bound is the sign
    /// that [`family_of`] is mis-grouping something.
    pub variadic_families: usize,
}

/// Which SQL this connection has admitted into Turso's statement cache.
///
/// Compiling SQL is the expensive half of running it — parse, plan, emit
/// bytecode — and every statement this app issues is one a screen re-runs. So
/// the question here is not "is this query worth caching" but "can this call
/// site only ever produce a bounded set of SQL texts": Turso's cache never
/// evicts, so one call site minting a text per call would grow it for ever.
/// One-shot SQL doesn't come through here at all — migrations run as
/// `batch_execute`, which is Turso's own multi-statement path.
///
/// Three things answer that question, in order, and they are deliberately not
/// one thing — see [`StatementCache::admit`].
struct StatementCache {
    size: CacheSize,
    /// Texts admitted, as hashes rather than the SQL itself: Turso already
    /// holds the text as its own key, and this side only has to answer
    /// "admitted before?".
    admitted: FxHashSet<u64>,
    /// Families seen once or found variadic. Bounded by `probation_order`.
    families: FxHashMap<u64, Family>,
    /// Insertion order for `families`, so the map can be trimmed to
    /// [`MAX_PROBATION_FAMILIES`] oldest-first.
    probation_order: VecDeque<u64>,
    /// Whether the cap has already been reported. The counter carries how
    /// often; the log line only has to say *that*, once, because a warning
    /// per execution on a write path is itself a performance problem.
    warned_at_cap: bool,
    hits: u64,
    uncached: u64,
    refused_at_cap: u64,
    variadic_families: usize,
}

impl Default for StatementCache {
    fn default() -> Self {
        Self {
            size: CacheSize::Unbounded,
            admitted: FxHashSet::default(),
            families: FxHashMap::default(),
            probation_order: VecDeque::new(),
            warned_at_cap: false,
            hits: 0,
            uncached: 0,
            refused_at_cap: 0,
            variadic_families: 0,
        }
    }
}

impl StatementCache {
    fn stats(&self) -> StatementCacheStats {
        StatementCacheStats {
            admitted: self.admitted.len(),
            hits: self.hits,
            uncached: self.uncached,
            refused_at_cap: self.refused_at_cap,
            variadic_families: self.variadic_families,
        }
    }

    /// Whether this SQL should be compiled through Turso's cache.
    ///
    /// `safe_to_cache` is diesel's verdict, from
    /// [`QueryFragment::is_safe_to_cache_prepared`]. It is taken as evidence
    /// rather than as the decision, because it is *conservative in a way that
    /// costs us the app's whole write path*: diesel returns false
    /// unconditionally for `UPDATE` (`UpdateStatement::walk_ast` calls
    /// `unsafe_to_cache_prepared`) and for `ON CONFLICT … DO UPDATE`, on the
    /// grounds that an `AsChangeset` with N optional columns can emit 2^N
    /// different `SET` lists. That reasoning is about *diesel's* cache, which
    /// is keyed by `TypeId` — one Rust type, many texts, so type-keying
    /// genuinely breaks down. Ours is keyed by the SQL text, which is also
    /// what Turso's `prepare_cached` keys by, so the objection does not carry
    /// over: 2^N texts would simply be 2^N keys, bounded by the cap below.
    /// Taking diesel's false as final left every `UPDATE` and every upsert in
    /// this app — 131 statements — recompiling its program on every call.
    ///
    /// So a statement diesel vetoes gets a second question, and it takes two
    /// parts because neither part alone is enough:
    ///
    /// - **Recurrence.** A text admitted only on its *second* sighting can
    ///   never be one a `format!` built out of runtime values, because those
    ///   are all different. This is what keeps a `sql_query`/`dsl::sql`
    ///   fragment interpolating an id out of the cache, while one whose text
    ///   is static goes in. It is not enough on its own: an `IN (?, ?, ?)`
    ///   over a three-element list recurs perfectly well, and so does every
    ///   other length.
    /// - **Family.** [`family_of`] collapses runs of adjacent placeholders,
    ///   so all the lengths of one `eq_any` — and all the row counts of one
    ///   multi-row insert — share a key. The second *distinct* text under
    ///   that key is proof the call site is variadic, and closes it for good.
    ///   Not enough on its own either: a `format!` site mints a fresh family
    ///   per call, so it never produces a second text under one key.
    ///
    /// A variadic call site can therefore cost one slot before it is
    /// recognised — see [`Family::Variadic`] — but never a slot per length,
    /// which is the growth that mattered.
    ///
    /// Together they admit exactly the bounded-text shapes: a fixed `.set()`
    /// tuple renders one text however its binds move, and an upsert renders
    /// one text even though its `VALUES (?, ?)` looks variadic in isolation —
    /// which is why the variadic test is evidence over sightings rather than
    /// a pattern match on a single text.
    ///
    /// Under all of it, [`MAX_CACHED_STATEMENTS`] holds a hard line, because
    /// a backstop against a shape we didn't anticipate is cheaper than the
    /// unbounded growth it prevents — an `AsChangeset` with optional columns
    /// would be exactly that shape, and this is what bounds it. A hash
    /// collision would let one statement in on another's slot, which costs
    /// nothing: Turso keys its cache by the SQL text itself, so a wrong
    /// answer here can only change *whether* a statement is cached, never
    /// which program runs.
    fn admit(&mut self, sql: &str, safe_to_cache: bool) -> bool {
        if matches!(self.size, CacheSize::Disabled) {
            self.uncached += 1;
            return false;
        }
        let text = hash_of(sql);
        // Already compiled into Turso's cache. Checked before anything else,
        // both because it is the common case and because a family that turns
        // variadic later must not push out a program Turso is already
        // holding: refusing it would pay the compile again for nothing.
        if self.admitted.contains(&text) {
            self.hits += 1;
            return true;
        }
        if !safe_to_cache && !self.recurs(sql, text) {
            self.uncached += 1;
            return false;
        }
        if self.admitted.len() >= MAX_CACHED_STATEMENTS {
            self.refused_at_cap += 1;
            if !self.warned_at_cap {
                self.warned_at_cap = true;
                tracing::warn!(
                    cap = MAX_CACHED_STATEMENTS,
                    "turso statement cache is full; further statements recompile on \
                     every execution. Either a query is minting SQL texts from runtime data \
                     or the app has outgrown the cap."
                );
            }
            self.uncached += 1;
            return false;
        }
        self.admitted.insert(text);
        true
    }

    /// Second sighting of this exact text, from a family that has only ever
    /// produced it. See [`StatementCache::admit`] for why both halves matter.
    fn recurs(&mut self, sql: &str, text: u64) -> bool {
        let family = family_of(sql);
        match self.families.get(&family) {
            Some(Family::Variadic) => false,
            Some(Family::Single(seen)) if *seen == text => true,
            Some(Family::Single(_)) => {
                self.families.insert(family, Family::Variadic);
                self.variadic_families += 1;
                false
            }
            None => {
                self.families.insert(family, Family::Single(text));
                self.probation_order.push_back(family);
                self.trim_probation();
                false
            }
        }
    }

    /// Forget the oldest families past the bound — from the queue and from
    /// the map together, which is the only thing that bounds either.
    ///
    /// Nothing else ever removes a family. Admission in particular does not:
    /// a `Single` entry is kept precisely so that a second, different text can
    /// contradict it, which is why an `eq_any` cannot buy a slot per list
    /// length (see [`Family::Single`]). So `probation_order` grows by one
    /// entry per never-before-seen family and shrinks only here, and the map
    /// is bounded because it is trimmed in step with the queue.
    ///
    /// A `Variadic` mark can be forgotten this way and then have to be
    /// re-learned, which costs one wasted slot in the worst case — the cap
    /// covers that, and holding variadic families for ever would reintroduce
    /// the unbounded map this bound exists to prevent.
    fn trim_probation(&mut self) {
        while self.probation_order.len() > MAX_PROBATION_FAMILIES {
            if let Some(oldest) = self.probation_order.pop_front() {
                self.families.remove(&oldest);
            }
        }
    }
}

/// Async connection to a Turso database.
#[allow(missing_debug_implementations)]
pub struct TursoConnection {
    conn: turso::Connection,
    // Keep the Database alive for the connection's lifetime.
    _db: turso::Database,
    transaction_state: AnsiAsyncTransactionManager,
    instrumentation: Option<Box<dyn Instrumentation>>,
    statement_cache: StatementCache,
}

impl TursoConnection {
    /// Access the inner `turso::Connection`. Useful for UNION/STRUCT
    /// queries that the diesel DSL can't express (and for tests).
    pub fn raw(&self) -> &turso::Connection {
        &self.conn
    }

    /// Open a Turso connection, optionally enabling multiprocess WAL.
    ///
    /// Multiprocess WAL lets a second process (the `fifteen db` CLI) read a
    /// database while the app holds it open. The current turso build has a
    /// checkpoint-reconciliation bug in that mode that aborts high-write-volume
    /// databases with "shared WAL frame ids must increase monotonically", so
    /// single-process stores opt out. In-memory backends (`:memory:` or an
    /// empty path) reject the flag, so it's never set for them.
    ///
    /// Foreign keys are not a parameter. There used to be a door that left
    /// them off, for schema migrations, on the reasoning that a table rebuild
    /// cannot run under enforcement — but a migration that needs them off
    /// says so in its own SQL, and a *connection* that hands them out off
    /// makes the state a migration runs under depend on which call site
    /// opened it. See the paragraph below for why the default is not
    /// "whatever the engine does".
    async fn open(database_url: &str, multiprocess_wal: bool) -> ConnectionResult<Self> {
        let is_in_memory = database_url.is_empty() || database_url == ":memory:";
        let mut builder = turso::Builder::new_local(database_url)
            .experimental_strict(true)
            .experimental_custom_types(true)
            .experimental_generated_columns(true)
            .experimental_index_method(true);
        if multiprocess_wal && !is_in_memory {
            builder = builder.experimental_multiprocess_wal(true);
        }
        let db = builder.build().await.map_err(turso_to_connection)?;
        let conn = db.connect().map_err(turso_to_connection)?;
        let mut conn = Self {
            conn,
            _db: db,
            transaction_state: AnsiAsyncTransactionManager::default(),
            instrumentation: None,
            statement_cache: StatementCache::default(),
        };

        // `PRAGMA foreign_keys` is per *connection*, and Turso — like SQLite —
        // defaults it off. A connection that never runs it accepts writes that
        // violate every `REFERENCES` in the schema, silently, for as long as it
        // lives; the symptom is orphan rows nobody notices for a month. That
        // makes "did this call site remember?" the wrong question to have to
        // ask, so establishing a connection answers it here instead. The
        // pragma has no transaction to be a no-op inside at this point, and it
        // costs one statement per connection.
        //
        // Run as a batch rather than through `execute`, which would offer the
        // statement to the cache: this one runs once per connection, so it
        // would hold a slot for ever and land in every count of what the cache
        // admitted — including the perf runner's.
        //
        // Spelled out as text because a pragma takes no bind parameters
        // (`PRAGMA foreign_keys = ?` is a parse error). The text is worth
        // reading twice before it changes: `PRAGMA foreign_key = ON`
        // (singular) is not an error, it is an unknown pragma, which is a
        // silent no-op — and the symptom is orphan rows nobody notices for a
        // month. `tests/turso/foreign_keys.rs` writes a row that must be
        // rejected, so a misspelling here turns the suite red.
        conn.batch_execute("PRAGMA foreign_keys = ON")
            .await
            .map_err(crate::ConnectionError::CouldntSetupConfiguration)?;
        Ok(conn)
    }

    /// Establish a single-process connection, with multiprocess WAL disabled.
    ///
    /// Use this for app-only stores (signal.db, whatsapp.db) that no second
    /// process opens while the app runs: it dodges the multiprocess-WAL
    /// checkpoint bug that aborts high-write-volume databases. meta.db keeps
    /// multiprocess WAL enabled via the `establish` trait method so the
    /// `fifteen db` CLI can read it live. (The CLI can still query the
    /// single-process stores while the app is closed.)
    pub async fn establish_single_process(database_url: &str) -> ConnectionResult<Self> {
        Self::open(database_url, false).await
    }

    /// What the statement cache has admitted, hit and refused so far.
    ///
    /// Exposed because "did that go through the cache?" is otherwise
    /// unanswerable from outside: a statement compiled fresh returns exactly
    /// the same rows as one served from the cache, only slower. The perf
    /// runner's `queries` scenario reads it, and the tests assert on it —
    /// without which a regression in [`StatementCache::admit`] would leave
    /// every test still passing.
    pub fn statement_cache_stats(&self) -> StatementCacheStats {
        self.statement_cache.stats()
    }

    /// Whether a just-serialised query should be compiled through Turso's
    /// statement cache. Takes the serialisation result so the decision — the
    /// one part of running a query that needs `&mut self` — is made before
    /// the connection is cloned into the future that runs it. SQL that failed
    /// to serialise is never cached; it is about to become an error.
    fn admit(&mut self, prepared: &QueryResult<Prepared>) -> bool {
        match prepared {
            Ok(prepared) => self
                .statement_cache
                .admit(&prepared.sql, prepared.safe_to_cache),
            Err(_) => false,
        }
    }
}

/// A diesel query fragment, serialised and ready for Turso.
struct Prepared {
    sql: String,
    binds: Vec<turso::Value>,
    /// Diesel's verdict on whether this query's type bounds its SQL text —
    /// see [`StatementCache::admit`], which is the only reader.
    safe_to_cache: bool,
}

/// Serialise a diesel query fragment to SQL, bind values, and whether it is
/// the sort of query worth caching.
fn prepare<T>(source: T) -> QueryResult<Prepared>
where
    T: QueryFragment<Turso>,
{
    let mut qb = TursoQueryBuilder::default();
    source.to_sql(&mut qb, &Turso)?;
    let mut binds = TursoBindCollector::default();
    source.collect_binds(&mut binds, &mut (), &Turso)?;
    Ok(Prepared {
        sql: qb.finish(),
        binds: binds.into_values(),
        safe_to_cache: source.is_safe_to_cache_prepared(&Turso)?,
    })
}

/// Compile `sql` on `conn`, through Turso's statement cache when `cached`.
///
/// This is `turso::Connection::{query, execute}` split in half so the compile
/// step can be the cached one. What those two do either side of it — take the
/// connection's dangling-transaction action first — has nothing to do here:
/// that action belongs to a dropped `turso::Transaction`, and transactions in
/// this crate are `BEGIN` / `COMMIT` / `SAVEPOINT` text emitted by
/// [`AnsiAsyncTransactionManager`], so the connection's dangling-transaction
/// state is never anything but `Ignore`.
async fn compile(
    conn: &turso::Connection,
    sql: &str,
    cached: bool,
) -> QueryResult<turso::Statement> {
    let statement = if cached {
        conn.prepare_cached(sql).await
    } else {
        conn.prepare(sql).await
    };
    statement.map_err(turso_to_diesel)
}

impl SimpleAsyncConnection for TursoConnection {
    fn batch_execute(&mut self, query: &str) -> impl Future<Output = QueryResult<()>> + Send {
        // Delegate to Turso's native multi-statement executor — it uses the
        // real SQL parser, so it handles `--` comments, string literals, and
        // `CREATE TRIGGER … BEGIN … END;` bodies that our hand-rolled
        // `;`-splitter couldn't.
        let query = query.to_owned();
        let conn = self.conn.clone();
        let transaction_state = &mut self.transaction_state;
        async move {
            let result = conn.execute_batch(&query).await.map_err(turso_to_diesel);
            if result.is_err() {
                flag_rollback_if_turso_still_holds_the_transaction(&conn, transaction_state);
            }
            result
        }
    }
}

/// Tell the transaction manager that a rollback is owed, when Turso says the
/// transaction that just failed is still open.
///
/// This is the Turso half of a contract diesel's other backends also
/// implement — see the `update_transaction_manager_status` in
/// `pg/async_connection` and `mysql/async_connection` — but the condition is
/// a different one, and the case it exists for is the one that hurts most.
///
/// Turso does not close a transaction on error. Not on a constraint
/// violation, not on a parse error, and — the case this function is really
/// about — *not on a failed `COMMIT`*: `TxOp::Commit` in
/// `core/vdbe/execute.rs` pre-checks deferred foreign keys and returns early
/// on a violation, deliberately leaving `auto_commit` false so the caller can
/// decide what to do. Turso is right to do that, and it means a `COMMIT` that
/// returns an error has *not* ended the transaction.
///
/// Without this, that left the connection in a state nothing could get it out
/// of. Diesel's depth would come back to zero while Turso still held an open
/// transaction, so every later statement — plain autocommit writes, the ones
/// the caller has every reason to think are durable — joined the orphan
/// instead. They read back correctly on that connection, because they really
/// are in the transaction, and they are gone the moment it closes. One
/// long-lived connection, which is exactly how the app's meta database is
/// used, could quietly lose every write it made after a single deferred-FK
/// violation.
///
/// Setting `requires_rollback_maybe_up_to_top_level` is what routes
/// [`AnsiAsyncTransactionManager::commit_transaction`] into its repair path:
/// it rolls back, unwinds the depth, and reports
/// [`RollbackErrorOnCommit`](crate::result::Error::RollbackErrorOnCommit) if
/// even that fails, rather than returning to the caller with the database
/// still mid-transaction.
///
/// The `is_autocommit` guard is not belt-and-braces. Turso has one commit
/// failure that *does* unwind — an abandoned write statement, which triggers
/// `rollback_manual_txn_cleanup` — and asking for a `ROLLBACK` after that one
/// would fail with "cannot rollback - no transaction is active" and put the
/// manager in an error state permanently. So we ask Turso which of the two
/// happened rather than assuming, and when it has already unwound we leave
/// the flag alone: the manager then simply drops the depth, and the two
/// agree again.
fn flag_rollback_if_turso_still_holds_the_transaction(
    conn: &turso::Connection,
    transaction_state: &mut AnsiAsyncTransactionManager,
) {
    // A connection too broken to answer is a connection we should not be
    // guessing about; leaving the flag clear keeps the manager on the path
    // that emits no further SQL.
    if conn.is_autocommit().unwrap_or(true) {
        return;
    }
    transaction_state
        .status
        .set_requires_rollback_maybe_up_to_top_level(true);
}

impl AsyncConnectionCore for TursoConnection {
    type Backend = Turso;

    type ExecuteFuture<'conn, 'query> =
        Pin<Box<dyn Future<Output = QueryResult<usize>> + Send + 'conn>>;
    type LoadFuture<'conn, 'query> =
        Pin<Box<dyn Future<Output = QueryResult<Self::Stream<'conn, 'query>>> + Send + 'conn>>;
    type Stream<'conn, 'query> = BoxStream<'conn, QueryResult<Self::Row<'conn, 'query>>>;
    type Row<'conn, 'query> = TursoRow<'conn>;

    fn load<'conn, 'query, T>(&'conn mut self, source: T) -> Self::LoadFuture<'conn, 'query>
    where
        T: AsQuery + 'query,
        T::Query: QueryFragment<Self::Backend> + QueryId + 'query,
    {
        let prepared = prepare(source.as_query());
        let cached = self.admit(&prepared);
        let conn = self.conn.clone();
        Box::pin(async move {
            let Prepared { sql, binds, .. } = prepared?;
            let mut statement = compile(&conn, &sql, cached).await?;
            let rows = statement.query(binds).await.map_err(turso_to_diesel)?;
            // Share column names across every row of this result set via
            // Arc so we don't clone the Vec<String> per row.
            let column_names: Arc<[String]> = rows.column_names().into();
            let col_count = column_names.len();

            // Lazy row-at-a-time stream: each poll steps turso's Rows
            // once and yields a single buffered TursoRow. Peak memory is
            // O(one row) rather than O(entire result set).
            //
            // State is `None` once the stream is exhausted or hit an
            // error — the `?` below terminates the unfold.
            let state = Some((rows, column_names, col_count));
            let s = stream::unfold(state, |state| async move {
                let (mut rows, col_names, col_count) = state?;
                match rows.next().await {
                    Ok(Some(row)) => {
                        let mut values = Vec::with_capacity(col_count);
                        for i in 0..col_count {
                            match row.get_value(i) {
                                Ok(v) => values.push(v),
                                Err(e) => {
                                    return Some((Err(turso_to_diesel(e)), None));
                                }
                            }
                        }
                        let next = Some((rows, Arc::clone(&col_names), col_count));
                        Some((Ok(TursoRow::new(values, col_names)), next))
                    }
                    Ok(None) => None,
                    Err(e) => Some((Err(turso_to_diesel(e)), None)),
                }
            });
            Ok(Box::pin(s) as BoxStream<'_, _>)
        })
    }

    fn execute_returning_count<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> Self::ExecuteFuture<'conn, 'query>
    where
        T: QueryFragment<Self::Backend> + QueryId + 'query,
    {
        let prepared = prepare(source);
        let cached = self.admit(&prepared);
        let conn = self.conn.clone();
        Box::pin(async move {
            let Prepared { sql, binds, .. } = prepared?;
            let mut statement = compile(&conn, &sql, cached).await?;
            // Step through `query`, not `execute`, and drain whatever comes
            // back. `turso::Statement::execute` steps with `columns: None`,
            // and a step that yields a row in that mode is
            // `Misuse("unexpected row during execution")` — raised *after*
            // the statement has already run. So `.returning(…).execute()`
            // and a bare `SELECT … .execute()` both reported an error for a
            // statement that had taken effect, and a caller that reads `Err`
            // as "nothing happened" and retries would apply the write twice.
            // Diesel's other backends return a row count from both, so this
            // was a Turso-only trap on a completely ordinary query shape.
            //
            // Draining costs nothing for the DML case (a `RETURNING`-less
            // statement yields no rows) and is what makes the count correct
            // for the rest: `n_change` is only final once the statement has
            // reached `Done`.
            let mut rows = statement.query(binds).await.map_err(turso_to_diesel)?;
            while rows.next().await.map_err(turso_to_diesel)?.is_some() {}
            Ok(statement.n_change() as usize)
        })
    }
}

impl AsyncConnection for TursoConnection {
    /// Diesel's ANSI manager, unmodified — `BEGIN` at the top level and
    /// `SAVEPOINT diesel_savepoint_N` beneath it.
    ///
    /// This used to be a hand-rolled `TursoTransactionManager`, justified by
    /// "Turso doesn't support SAVEPOINTs". That was true once and is not now:
    /// `SAVEPOINT`, `ROLLBACK TO SAVEPOINT` and `RELEASE SAVEPOINT` are all
    /// implemented on the Turso revision this crate pins (see
    /// `core/translate/rollback.rs`), and the acceptance suite exercises them
    /// against a real database rather than taking anyone's word for it.
    ///
    /// Three things came back with the ANSI manager, and each of them was a
    /// way to lose data quietly:
    ///
    /// 1. **Nesting means what diesel documents it to mean.** The bespoke
    ///    manager folded every nested `.transaction()` into the outer one and
    ///    "poisoned" the outer if an inner callback returned `Err` — so an
    ///    inner rollback threw away the outer transaction's work as well,
    ///    including work done before the inner one began. Under savepoints an
    ///    inner rollback undoes the inner block and nothing else, which is
    ///    what every diesel backend does and what callers write their code
    ///    against.
    /// 2. **A failed `COMMIT` is repaired instead of latching.** The old
    ///    manager put the status in `InError` with the depth left where it
    ///    was, and nothing public could clear it. See
    ///    [`flag_rollback_if_turso_still_holds_the_transaction`], which is
    ///    the Turso-specific half of that repair.
    /// 3. **A cancelled transaction future is detectable.** The ANSI manager
    ///    wraps every `BEGIN`/`COMMIT`/`ROLLBACK` await in a critical block
    ///    whose flag stays set if the future is dropped mid-statement, and
    ///    overrides `is_broken_transaction_manager` to report it — so a pool
    ///    retires that connection instead of handing it out with the
    ///    database in a state nobody knows.
    ///
    /// The one thing the bespoke manager did that this does not is treat
    /// "cannot commit - no transaction is active" as a soft success, on the
    /// theory that Turso's deferred transaction might never latch for a
    /// read-only block. It does latch: `BEGIN` sets `auto_commit` false
    /// unconditionally, so a read-only `BEGIN` … `COMMIT` returns `Ok`, and
    /// that branch was unreachable on this revision. It went with the
    /// manager, along with the `last_commit_did_not_latch` diagnostic that
    /// consumers read it through.
    type TransactionManager = AnsiAsyncTransactionManager;

    fn establish(database_url: &str) -> impl Future<Output = ConnectionResult<Self>> + Send {
        let url = database_url.to_owned();
        // meta.db keeps multiprocess WAL enabled so the `fifteen db` CLI can
        // read it while the app runs. Single-process stores (signal.db,
        // whatsapp.db) use `establish_single_process` to dodge the
        // multiprocess-WAL checkpoint bug. Every door enforces foreign keys;
        // there is no longer one that doesn't.
        async move { Self::open(&url, true).await }
    }

    fn transaction_state(
        &mut self,
    ) -> &mut <Self::TransactionManager as crate::connection::AsyncTransactionManager<Self>>::TransactionStateData
    {
        &mut self.transaction_state
    }

    fn instrumentation(&mut self) -> &mut dyn Instrumentation {
        self.instrumentation
            .get_or_insert_with(|| Box::new(NoopInstrumentation))
            .as_mut()
    }

    fn set_instrumentation(&mut self, instrumentation: impl Instrumentation) {
        self.instrumentation = Some(Box::new(instrumentation));
    }

    /// `Disabled` stops promoting statements from here on. Statements already
    /// promoted stay in Turso's cache — it exposes no way to drop them — so
    /// this is "cache no more", not "empty the cache".
    fn set_prepared_statement_cache_size(&mut self, size: CacheSize) {
        self.statement_cache.size = size;
    }
}

/// Lets a `TursoConnection` be pooled by
/// [`AsyncDieselConnectionManager`](crate::pooled_connection::AsyncDieselConnectionManager),
/// and so by bb8, deadpool or mobc.
///
/// This has no consumer inside this repository — the pools are exercised by
/// the application that vendors this fork, whose writer pool is an
/// `AsyncDieselConnectionManager<TursoConnection>` behind deadpool. It was
/// deleted once on the strength of the in-tree search and had to come back,
/// so: the bound is only checked where a manager is instantiated, and
/// nothing here instantiates one.
///
/// # Why there is no `ping` override
///
/// The default is right, and it is right for a reason worth writing down
/// rather than leaving as an absence.
///
/// [`RecyclingMethod::Verified`](crate::pooled_connection::RecyclingMethod::Verified),
/// the default, runs `SELECT 1` through
/// [`execute`](crate::query_dsl::async_run_query_dsl::RunQueryDsl::execute).
/// That is a statement which returns a row being run for its row *count* —
/// the exact shape `execute_returning_count` used to refuse on this backend,
/// with `Misuse("unexpected row during execution")` raised after the
/// statement had already run. Had the pool arrived first, `ping` would have
/// reported every healthy connection as dead. It steps the statement to
/// `Done` now, so the default works.
///
/// Nor is there anything cheaper to substitute. The checks other backends
/// spend a `ping` on — that the socket is still open, that the server has not
/// timed the session out, that a failover has not moved the primary — do not
/// exist for a database that is a file in this process. `SELECT 1` compiles
/// out of Turso's statement cache after the first checkout and steps no
/// pages, so what it costs is a VDBE call, and what it buys is the one thing
/// still worth knowing: that the connection answers at all.
///
/// The Turso-specific hazard a query *cannot* reveal is an open transaction,
/// and that belongs in
/// [`is_broken`](crate::pooled_connection::PoolableConnection::is_broken),
/// which every pool consults and which costs nothing.
#[cfg(feature = "async-pool")]
impl crate::pooled_connection::PoolableConnection for TursoConnection {
    /// Broken when diesel's transaction manager says so, or when Turso is
    /// still holding a transaction diesel does not know about.
    ///
    /// The first half is [`AnsiAsyncTransactionManager`]'s override: a
    /// transaction future dropped mid-`BEGIN`/`COMMIT` leaves a flag set that
    /// nothing clears, and one dropped inside the callback leaves the depth
    /// behind. Both mean the caller cannot say what state the database is in,
    /// and the answer to that is to retire the connection rather than reuse
    /// it. See `tests/turso/transaction_recovery.rs`.
    ///
    /// The second half is Turso's own, and no other backend in this crate has
    /// to consider it, because no other backend hands out its driver
    /// connection: [`TursoConnection::raw`] is the documented escape hatch for
    /// the UNION and STRUCT queries the DSL cannot express, and SQL issued
    /// through it is invisible to the transaction manager. `auto_commit` is
    /// the engine's own answer to "am I in a transaction", so it catches the
    /// disagreement that depth cannot.
    ///
    /// It is worth catching because of what the disagreement does once a pool
    /// is involved. A single long-lived connection at least keeps an orphaned
    /// transaction to the caller that opened it — see
    /// [`flag_rollback_if_turso_still_holds_the_transaction`]. A pool hands
    /// that connection to the *next* caller, whose ordinary autocommit writes
    /// then join a transaction nobody is going to commit: they return `Ok`,
    /// they read back correctly for as long as the connection lives, and they
    /// are gone when it closes. Nothing anywhere returns an error. One
    /// `is_autocommit` call on the way back into the pool is a cheap price for
    /// ruling that out.
    ///
    /// It is also what makes
    /// [`RecyclingMethod::Fast`](crate::pooled_connection::RecyclingMethod::Fast)
    /// safe here, which matters more than it does elsewhere: `Fast` skips
    /// [`PoolableConnection::ping`](crate::pooled_connection::PoolableConnection::ping)
    /// altogether, and for an embedded database it is the recycling method
    /// that makes sense — so `is_broken` is then the only thing standing
    /// between a returned connection and the next caller.
    ///
    /// A connection too broken to answer `is_autocommit` is reported broken
    /// rather than assumed healthy, the same direction `AsyncPgConnection`
    /// takes with `is_closed`.
    fn is_broken(&mut self) -> bool {
        use crate::connection::AsyncTransactionManager;

        Self::TransactionManager::is_broken_transaction_manager(self)
            || !self.conn.is_autocommit().unwrap_or(false)
    }
}

struct NoopInstrumentation;
impl Instrumentation for NoopInstrumentation {
    fn on_connection_event(&mut self, _event: crate::connection::InstrumentationEvent<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "SELECT id FROM t WHERE id = ?";
    const B: &str = "SELECT name FROM t WHERE id = ?";

    /// The verdict a typed `SELECT` or single-row `INSERT` carries.
    const SAFE: bool = true;
    /// The verdict diesel gives every `UPDATE`, every `ON CONFLICT … DO
    /// UPDATE`, every `eq_any`, every multi-row insert and every
    /// `SqlLiteral` alike — the bit this cache refuses to read as final.
    const VETOED: bool = false;

    /// A fixed `.set()` tuple as diesel renders it: one text, whatever the
    /// binds. Diesel vetoes it anyway.
    const UPDATE: &str = r#"UPDATE "t" SET "name" = ?, "n" = ? WHERE ("t"."id" = ?)"#;
    /// `ON CONFLICT … DO UPDATE`, likewise vetoed, and note the `VALUES (?,
    /// ?)` that a pattern-matching filter would mistake for a variadic run.
    const UPSERT: &str = r#"INSERT INTO "t" ("id", "name") VALUES (?, ?) ON CONFLICT ("id") DO UPDATE SET "name" = ?"#;

    fn in_list(n: usize) -> String {
        let list = vec!["?"; n].join(", ");
        format!("SELECT id FROM t WHERE id IN ({list})")
    }

    #[test]
    fn admits_what_diesel_blesses_from_the_first_sighting() {
        let mut cache = StatementCache::default();
        assert!(
            cache.admit(A, SAFE),
            "the first run is compiled into the cache"
        );
        assert!(
            cache.admit(A, SAFE),
            "and every run after it reads from there"
        );
        assert!(
            cache.admit(B, SAFE),
            "a different statement gets its own slot"
        );
        assert_eq!(cache.stats().admitted, 2);
        assert_eq!(cache.stats().hits, 1);
    }

    /// The defect this policy exists for. Diesel vetoes every `UPDATE`
    /// outright, so under the old "diesel's verdict is the decision" rule
    /// these never reached Turso's cache at all.
    #[test]
    fn admits_a_vetoed_update_on_its_second_sighting() {
        let mut cache = StatementCache::default();
        assert!(
            !cache.admit(UPDATE, VETOED),
            "the first sighting proves nothing — it could be a format! text"
        );
        assert!(
            cache.admit(UPDATE, VETOED),
            "the second is the recurrence that proves the text is bounded"
        );
        assert!(cache.admit(UPDATE, VETOED), "and every one after it");
        assert_eq!(cache.stats().admitted, 1);
        assert_eq!(cache.stats().hits, 1);
    }

    /// An upsert renders `VALUES (?, ?)` — which *looks* variadic in
    /// isolation. It is admitted because the evidence says otherwise: the
    /// call site only ever emits the one text.
    #[test]
    fn admits_an_upsert_despite_its_placeholder_run() {
        let mut cache = StatementCache::default();
        assert!(!cache.admit(UPSERT, VETOED));
        assert!(cache.admit(UPSERT, VETOED));
        assert_eq!(cache.stats().admitted, 1);
        assert_eq!(cache.stats().variadic_families, 0);
    }

    /// The case the family test exists for. `eq_any` over a runtime list
    /// emits a placeholder per element, so each length is its own SQL text —
    /// and each one *recurs*, which is exactly why recurrence alone could not
    /// have been the whole rule. None of them may take a slot in a cache that
    /// never evicts.
    #[test]
    fn an_in_list_costs_one_slot_however_many_lengths_it_has() {
        let mut cache = StatementCache::default();
        for n in 1..=100 {
            let sql = in_list(n);
            for _ in 0..5 {
                cache.admit(&sql, VETOED);
            }
            assert!(
                cache.stats().admitted <= 1,
                "after {n} lengths the cache still holds at most the one that \
                 recurred before the call site was recognised"
            );
        }
        assert_eq!(
            cache.stats().variadic_families,
            1,
            "all hundred lengths were recognised as one call site"
        );
        // Nothing later gets in either: the family is closed for good.
        for n in [3, 17, 100] {
            assert!(!cache.admit(&in_list(n), VETOED));
        }
        assert_eq!(cache.stats().admitted, 1);

        // A safe statement alongside them is unaffected.
        assert!(cache.admit(A, SAFE));
    }

    /// Interleaved lengths — the shape a real `eq_any` call site has — never
    /// get a slot at all, because the second length arrives before the first
    /// has recurred.
    #[test]
    fn interleaved_in_list_lengths_never_get_in() {
        let mut cache = StatementCache::default();
        for round in 0..50 {
            for n in 1..=8 {
                assert!(
                    !cache.admit(&in_list(n), VETOED),
                    "round {round}, length {n}"
                );
            }
        }
        assert_eq!(cache.stats().admitted, 0);
    }

    /// Multi-row inserts vary by row count the same way, and collapse to one
    /// family the same way — `VALUES (?, ?), (?, ?)` is one placeholder run.
    #[test]
    fn refuses_every_row_count_of_a_multi_row_insert() {
        let mut cache = StatementCache::default();
        for round in 0..20 {
            for rows in 1..=10 {
                let tuples = vec!["(?, ?)"; rows].join(", ");
                let sql = format!(r#"INSERT INTO "t" ("id", "name") VALUES {tuples}"#);
                assert!(!cache.admit(&sql, VETOED), "round {round}, {rows} rows");
            }
        }
        assert_eq!(cache.stats().admitted, 0);
        assert_eq!(
            cache.stats().variadic_families,
            1,
            "every row count is one call site, not ten"
        );
    }

    /// A statement whose text was built by `format!` is a fresh text every
    /// call, so it never recurs and never gets in — and, because each text is
    /// its own family, it never trips the variadic test either. Recurrence is
    /// the half of the rule that catches this one.
    #[test]
    fn refuses_a_text_that_is_never_the_same_twice() {
        let mut cache = StatementCache::default();
        for i in 0..2_000 {
            assert!(!cache.admit(&format!("SELECT * FROM t WHERE id = {i}"), VETOED));
        }
        assert_eq!(cache.stats().admitted, 0);
        assert!(
            cache.families.len() <= MAX_PROBATION_FAMILIES,
            "and probation stayed bounded rather than leaking one entry per call"
        );
        assert!(cache.probation_order.len() <= MAX_PROBATION_FAMILIES);
    }

    /// A `dsl::sql` fragment with static text is vetoed by diesel exactly like
    /// one built by `format!` — diesel cannot tell them apart. Recurrence can.
    ///
    /// The rule this demonstrates is "a text that recurs is admitted unless
    /// its *family* has produced a second text", not "static text is
    /// admitted". The two come apart, because [`family_of`] canonicalises
    /// bytes and does not parse: it has no idea where a string literal
    /// begins. Three statements as static as any —
    ///
    /// ```sql
    /// SELECT CAST('?'   AS TEXT)
    /// SELECT CAST('??'  AS TEXT)
    /// SELECT CAST('???' AS TEXT)
    /// ```
    ///
    /// differ only in the number of `?`s inside a literal, collapse to one
    /// family key, and so are read as one variadic call site: the second one
    /// seen closes the family and all three are refused for the life of the
    /// process.
    ///
    /// That is a missed cache, never a wrong answer — an uncached statement
    /// runs the same program, compiled again — and no shape diesel renders
    /// collides this way. Narrower and wider `SET` lists, column lists,
    /// chained `WHERE`s and upserts all keep a column name or an operator
    /// between their placeholders, which ends the run. So it is left as it is
    /// and written down here, rather than paid for with a SQL-aware scan on
    /// every statement the app executes.
    #[test]
    fn admits_a_static_sql_literal_and_refuses_an_interpolated_one() {
        let mut cache = StatementCache::default();
        let literal = r#"SELECT "kind" & 3 FROM "t" WHERE "id" = ?"#;
        assert!(!cache.admit(literal, VETOED));
        assert!(cache.admit(literal, VETOED), "static text recurs");

        for i in 0..50 {
            let interpolated = format!(r#"SELECT "kind" & {i} FROM "t""#);
            assert!(!cache.admit(&interpolated, VETOED));
        }
        assert_eq!(cache.stats().admitted, 1, "only the static one got in");
    }

    /// The counter-example to "static text is admitted", stated in
    /// [`admits_a_static_sql_literal_and_refuses_an_interpolated_one`]'s doc.
    ///
    /// [`family_of`] canonicalises bytes and does not parse SQL, so `?`s
    /// inside a string literal are collapsed like any other placeholder run.
    /// These three texts are as static as it gets, share one family key, and
    /// are all refused for the life of the process once the second one is
    /// seen. Pinned rather than fixed: it costs a missed cache and never a
    /// wrong answer, no shape diesel renders collides this way, and the fix
    /// would be a SQL-aware scan on every statement the app executes. If that
    /// trade ever stops being the right one, this test is the description of
    /// what changed.
    #[test]
    fn question_marks_inside_a_literal_are_read_as_a_placeholder_run() {
        let a = "SELECT CAST('?' AS TEXT)";
        let b = "SELECT CAST('??' AS TEXT)";
        let c = "SELECT CAST('???' AS TEXT)";
        assert_eq!(family_of(a), family_of(b), "a and b share a family");
        assert_eq!(family_of(a), family_of(c), "a and c share a family");
        let mut cache = StatementCache::default();
        assert!(!cache.admit(a, VETOED));
        assert!(!cache.admit(b, VETOED));
        assert!(!cache.admit(c, VETOED));
        assert!(
            !cache.admit(a, VETOED),
            "a is refused even though it recurs"
        );
        assert_eq!(cache.stats().admitted, 0);
    }

    #[test]
    fn stops_admitting_at_the_cap_and_says_so() {
        let mut cache = StatementCache::default();
        for i in 0..MAX_CACHED_STATEMENTS {
            assert!(cache.admit(&format!("SELECT {i}"), SAFE));
        }
        assert_eq!(cache.stats().admitted, MAX_CACHED_STATEMENTS);
        assert_eq!(cache.stats().refused_at_cap, 0);

        // Past the cap nothing new gets in — however many times it runs.
        let beyond = "SELECT 'past the cap'";
        assert!(!cache.admit(beyond, SAFE));
        assert!(!cache.admit(beyond, SAFE));
        assert_eq!(cache.stats().admitted, MAX_CACHED_STATEMENTS);
        assert_eq!(
            cache.stats().refused_at_cap,
            2,
            "and every refusal is counted, because nothing else would show it"
        );
        assert!(cache.warned_at_cap, "the first one also logged, once");

        // Statements admitted before the cap keep their slot.
        assert!(cache.admit("SELECT 0", SAFE));
    }

    /// A vetoed statement that recurs at the cap is refused like any other,
    /// and counted — the cap is what bounds an `AsChangeset` with optional
    /// columns, the one bounded-per-type-but-2^N-texts shape this policy
    /// cannot recognise from evidence.
    #[test]
    fn the_cap_bounds_the_recurrence_path_too() {
        let mut cache = StatementCache::default();
        for i in 0..MAX_CACHED_STATEMENTS {
            assert!(cache.admit(&format!("SELECT {i}"), SAFE));
        }
        assert!(!cache.admit(UPDATE, VETOED), "first sighting: probation");
        assert!(!cache.admit(UPDATE, VETOED), "recurs, but there is no room");
        assert_eq!(cache.stats().refused_at_cap, 1);
    }

    #[test]
    fn disabled_never_promotes() {
        let mut cache = StatementCache {
            size: CacheSize::Disabled,
            ..StatementCache::default()
        };
        assert!(!cache.admit(A, SAFE));
        assert!(!cache.admit(A, SAFE));
        assert_eq!(cache.stats().admitted, 0);
        assert!(
            cache.families.is_empty(),
            "disabled tracks nothing either — not even probation"
        );
        assert_eq!(cache.stats().uncached, 2);
    }

    /// The probation map is bounded, and forgetting from it is safe.
    ///
    /// This is the half of the design that keeps a `format!` call site from
    /// becoming a slow leak: every call mints a fresh family, and without the
    /// bound the map would grow one entry per call for the life of the
    /// process. Nothing else in this file would notice — the statements are
    /// correctly refused either way, so the only symptom is memory.
    #[test]
    fn probation_is_bounded_by_its_cap() {
        let mut cache = StatementCache::default();
        // Each of these is a distinct text under a distinct family, exactly
        // as `format!("… WHERE id = {n}")` would produce.
        for n in 0..(MAX_PROBATION_FAMILIES * 2) {
            assert!(
                !cache.admit(&format!("SELECT id FROM t{n} WHERE id = ?"), VETOED),
                "a text seen once is never admitted"
            );
        }
        assert!(
            cache.families.len() <= MAX_PROBATION_FAMILIES,
            "probation held {} families against a cap of {MAX_PROBATION_FAMILIES}",
            cache.families.len()
        );
        assert!(
            cache.probation_order.len() <= MAX_PROBATION_FAMILIES,
            "the insertion-order queue is bounded too, or it becomes the leak \
             the map no longer is"
        );
        assert_eq!(
            cache.stats().admitted,
            0,
            "and none of it got into Turso's cache"
        );
    }

    /// A family evicted from probation is re-learned rather than remembered
    /// wrongly.
    ///
    /// The cost is documented as "one wasted slot in the worst case", and
    /// this is what that looks like: a variadic family whose mark has aged
    /// out behaves like a family nobody has ever seen, so the next text under
    /// it is admitted on its second sighting. That is a slot spent on a
    /// statement which is perfectly good to cache — the point of the test is
    /// that it is *one* slot and the cap covers it, not that eviction is
    /// free.
    #[test]
    fn an_evicted_family_is_re_learned_not_mis_remembered() {
        let mut cache = StatementCache::default();
        // Establish a variadic family: two different lengths of one IN list.
        assert!(!cache.admit(&in_list(2), VETOED), "first sighting");
        assert!(!cache.admit(&in_list(5), VETOED), "second, different, text");
        assert_eq!(
            cache.stats().variadic_families,
            1,
            "the call site has been recognised as variadic"
        );

        // Age it out with unrelated traffic.
        for n in 0..(MAX_PROBATION_FAMILIES + 1) {
            cache.admit(&format!("SELECT id FROM t{n} WHERE id = ?"), VETOED);
        }

        // The mark is gone, so the family starts over — which is the
        // documented cost, not a defect.
        assert!(
            !cache.admit(&in_list(9), VETOED),
            "back to a first sighting"
        );
        assert!(
            cache.admit(&in_list(9), VETOED),
            "and the second sighting of that same text is admitted, spending \
             the one slot the design says it may"
        );
        assert_eq!(cache.stats().admitted, 1);
    }

    /// A disabled cache refuses everything and says so in the counters.
    #[test]
    fn a_disabled_cache_admits_nothing() {
        let mut cache = StatementCache {
            size: CacheSize::Disabled,
            ..Default::default()
        };
        assert!(!cache.admit(A, SAFE));
        assert!(!cache.admit(A, SAFE));
        assert_eq!(cache.stats().admitted, 0);
        assert_eq!(cache.stats().uncached, 2);
        assert_eq!(
            cache.stats().hits,
            0,
            "a disabled cache cannot report hits, or the perf report would \
             show a cache that is not running as one that is"
        );
    }

    /// The family key groups by placeholder-run arity and nothing else. This
    /// is the property the whole variadic test rests on, so it is asserted
    /// directly rather than only through `admit`.
    #[test]
    fn family_groups_placeholder_runs_and_only_those() {
        assert_eq!(
            family_of(&in_list(3)),
            family_of(&in_list(7)),
            "two lengths of one IN list are one family"
        );
        assert_ne!(
            family_of(&in_list(3)),
            family_of(r#"SELECT id FROM other WHERE id IN (?, ?, ?)"#),
            "a different table is a different family"
        );
        assert_ne!(
            family_of(UPDATE),
            family_of(r#"UPDATE "t" SET "name" = ? WHERE ("t"."id" = ?)"#),
            "two independent SET slots are not one run: a narrower SET is a \
             different family, not a shorter list"
        );
        assert_eq!(
            family_of(UPSERT),
            family_of(UPSERT),
            "and the key is a function of the text alone"
        );
    }
}
