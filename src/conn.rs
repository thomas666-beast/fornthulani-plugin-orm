use crate::error::{OrmError, OrmResult};
use crate::query::Query;
use crate::value::{Row, SqlValue};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecResult {
    pub rows_affected: u64,
    /// SQLite only (`last_insert_rowid`). PostgreSQL has no equivalent; the
    /// builder uses `INSERT ... RETURNING` there instead.
    pub last_insert_id: i64,
}

/// The SQL flavour a connection speaks. The builder always emits `?`
/// placeholders and double-quoted identifiers; the dialect only changes
/// the few things that really differ (DDL types, `ILIKE`, `FOR UPDATE`,
/// JSON paths, boolean literals, catalog queries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Sqlite,
    Postgres,
}

impl Dialect {
    pub(crate) fn bool_literal(self, b: bool) -> &'static str {
        match (self, b) {
            (Dialect::Postgres, true) => "TRUE",
            (Dialect::Postgres, false) => "FALSE",
            (Dialect::Sqlite, true) => "1",
            (Dialect::Sqlite, false) => "0",
        }
    }
}

/// Something that can run SQL: a connection, or a transaction in progress.
/// Every `*_on(&dyn Executor)` method takes one of these.
pub trait Executor {
    fn query(&self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>>;
    fn execute(&self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult>;
    fn execute_batch(&self, sql: &str) -> OrmResult<()>;
    /// Defaults to SQLite so third-party drivers written for 0.1 keep working.
    fn dialect(&self) -> Dialect {
        Dialect::Sqlite
    }
}

/// A database connection (or pool). Implement this to add a database; the
/// SQL the builder emits uses `?` placeholders and double-quoted
/// identifiers (translate in your driver if your database needs something
/// else, as the PostgreSQL driver does with `$1, $2, ...`).
pub trait Connection: Executor + Send + Sync {
    fn as_executor(&self) -> &dyn Executor;

    /// Run `f` atomically: commit on `Ok`, roll back on `Err` or panic.
    /// Use the executor you are handed *inside* `f`; calling the global
    /// `Db` / model methods there would use a different connection.
    fn transaction(&self, f: &mut dyn FnMut(&dyn Executor) -> OrmResult<()>) -> OrmResult<()>;
}

static CONN: RwLock<Option<Arc<dyn Connection>>> = RwLock::new(None);
static NAMED: LazyLock<RwLock<HashMap<String, Arc<dyn Connection>>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

// ----------------------------------------------------------------------
// query listener (Laravel's DB::listen)
// ----------------------------------------------------------------------

/// What a query listener sees. `params` are the bound values — treat them
/// as sensitive (they can contain passwords or tokens) if you log them.
pub struct QueryEvent<'a> {
    pub sql: &'a str,
    pub params: &'a [SqlValue],
    pub elapsed: Duration,
    pub dialect: Dialect,
    pub ok: bool,
}

pub type QueryListener = fn(&QueryEvent<'_>);

static LISTENER: RwLock<Option<QueryListener>> = RwLock::new(None);

/// Drivers call this around every statement.
pub fn observe<T>(
    dialect: Dialect,
    sql: &str,
    params: &[SqlValue],
    f: impl FnOnce() -> OrmResult<T>,
) -> OrmResult<T> {
    let listener = *LISTENER.read().unwrap_or_else(|e| e.into_inner());
    match listener {
        None => f(),
        Some(l) => {
            let start = Instant::now();
            let r = f();
            l(&QueryEvent {
                sql,
                params,
                elapsed: start.elapsed(),
                dialect,
                ok: r.is_ok(),
            });
            r
        }
    }
}

/// The global database handle (Laravel's `DB` facade).
pub struct Db;

impl Db {
    /// Install the default connection every model and `Db::table` call uses.
    pub fn set<C: Connection + 'static>(conn: C) {
        Self::set_arc(Arc::new(conn));
    }

    pub fn set_arc(conn: Arc<dyn Connection>) {
        let mut g = CONN.write().unwrap_or_else(|e| e.into_inner());
        *g = Some(conn);
    }

    pub fn get() -> OrmResult<Arc<dyn Connection>> {
        CONN.read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or(OrmError::NoConnection)
    }

    /// Register an extra, named connection (Laravel's `connections` config).
    /// Models pick it with `const CONNECTION: Option<&str> = Some("name")`,
    /// queries with `.on("name")`.
    pub fn add_connection<C: Connection + 'static>(name: &str, conn: C) {
        Self::add_connection_arc(name, Arc::new(conn));
    }

    pub fn add_connection_arc(name: &str, conn: Arc<dyn Connection>) {
        NAMED
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), conn);
    }

    pub fn has_connection(name: &str) -> bool {
        NAMED
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    /// A named connection (`None` = the default one).
    pub fn connection(name: Option<&str>) -> OrmResult<Arc<dyn Connection>> {
        match name {
            None => Self::get(),
            Some(n) => NAMED
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(n)
                .cloned()
                .ok_or_else(|| OrmError::Invalid(format!("unknown connection `{n}`"))),
        }
    }

    /// Start an untyped query on a table: `Db::table("users").where_eq(..).get()`.
    pub fn table(name: &str) -> Query {
        Query::new(name)
    }

    /// Run raw SQL. Takes `&'static str` on purpose: the statement must be
    /// a literal in your source, so request data cannot end up in it.
    /// Pass values through `params`.
    pub fn select(sql: &'static str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
        with_db(|ex| ex.query(sql, params))
    }

    /// Raw `INSERT`/`UPDATE`/`DELETE`/DDL (literal SQL only, see [`Db::select`]).
    pub fn statement(sql: &'static str, params: &[SqlValue]) -> OrmResult<ExecResult> {
        with_db(|ex| ex.execute(sql, params))
    }

    /// Run `f` in a transaction on the default connection.
    ///
    /// ```ignore
    /// Db::transaction(|tx| {
    ///     let order = Order::create_on(tx, &new_order)?;
    ///     Item::create_on(tx, &item_for(&order))?;
    ///     Ok(order)
    /// })?;
    /// ```
    pub fn transaction<T>(f: impl FnOnce(&dyn Executor) -> OrmResult<T>) -> OrmResult<T> {
        Self::transaction_on(None, f)
    }

    /// `transaction` on a named connection.
    pub fn transaction_on<T>(
        connection: Option<&str>,
        f: impl FnOnce(&dyn Executor) -> OrmResult<T>,
    ) -> OrmResult<T> {
        let conn = Db::connection(connection)?;
        transaction_in(&*conn, f)
    }

    /// Call `listener` after every statement (all connections). Use it for
    /// query logs, slow-query alerts or tests that count queries.
    pub fn listen(listener: QueryListener) {
        *LISTENER.write().unwrap_or_else(|e| e.into_inner()) = Some(listener);
    }

    pub fn stop_listening() {
        *LISTENER.write().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Run `f` in a transaction on `conn`, returning its value.
pub fn transaction_in<T>(
    conn: &dyn Connection,
    f: impl FnOnce(&dyn Executor) -> OrmResult<T>,
) -> OrmResult<T> {
    let mut f = Some(f);
    let mut out: Option<T> = None;
    conn.transaction(&mut |ex| {
        let f = f
            .take()
            .ok_or_else(|| OrmError::Invalid("transaction closure re-entered".into()))?;
        out = Some(f(ex)?);
        Ok(())
    })?;
    out.ok_or_else(|| OrmError::Db("transaction produced no value".into()))
}

/// Run `f` against the default connection.
pub(crate) fn with_db<T>(f: impl FnOnce(&dyn Executor) -> OrmResult<T>) -> OrmResult<T> {
    with_conn(None, f)
}

/// Run `f` against a named (or the default) connection.
pub(crate) fn with_conn<T>(
    name: Option<&str>,
    f: impl FnOnce(&dyn Executor) -> OrmResult<T>,
) -> OrmResult<T> {
    let conn = Db::connection(name)?;
    f(conn.as_executor())
}
