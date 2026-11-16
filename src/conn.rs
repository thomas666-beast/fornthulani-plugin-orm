use crate::error::{OrmError, OrmResult};
use crate::query::Query;
use crate::value::{Row, SqlValue};
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, Default)]
pub struct ExecResult {
    pub rows_affected: u64,
    pub last_insert_id: i64,
}

/// Something that can run SQL: a connection, or a transaction in progress.
/// Every `*_on(&dyn Executor)` method takes one of these.
pub trait Executor {
    fn query(&self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>>;
    fn execute(&self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult>;
    fn execute_batch(&self, sql: &str) -> OrmResult<()>;
}

/// A database connection. Implement this to add a database; the SQL the
/// builder emits uses `?` placeholders and double-quoted identifiers
/// (translate in your driver if your database needs something else).
pub trait Connection: Executor + Send + Sync {
    fn as_executor(&self) -> &dyn Executor;

    /// Run `f` atomically: commit on `Ok`, roll back on `Err` or panic.
    /// Use the executor you are handed *inside* `f`; calling the global
    /// `Db` / model methods there would try to take the connection again.
    fn transaction(&self, f: &mut dyn FnMut(&dyn Executor) -> OrmResult<()>) -> OrmResult<()>;
}

static CONN: RwLock<Option<Arc<dyn Connection>>> = RwLock::new(None);

/// The global database handle (Laravel's `DB` facade).
pub struct Db;

impl Db {
    /// Install the connection every model and `Db::table` call uses.
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

    /// Run `f` in a transaction.
    ///
    /// ```ignore
    /// Db::transaction(|tx| {
    ///     let order = Order::create_on(tx, &new_order)?;
    ///     Item::create_on(tx, &item_for(&order))?;
    ///     Ok(order)
    /// })?;
    /// ```
    pub fn transaction<T>(f: impl FnOnce(&dyn Executor) -> OrmResult<T>) -> OrmResult<T> {
        let conn = Db::get()?;
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
}

/// Run `f` against the global connection.
pub(crate) fn with_db<T>(f: impl FnOnce(&dyn Executor) -> OrmResult<T>) -> OrmResult<T> {
    let conn = Db::get()?;
    f(conn.as_executor())
}
