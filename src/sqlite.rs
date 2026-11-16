//! SQLite driver (feature `sqlite`, on by default).

use crate::conn::{Connection, Dialect, ExecResult, Executor, observe};
use crate::error::{OrmError, OrmResult};
use crate::value::{Row, SqlValue};
use rusqlite::types::{Value as SqlRaw, ValueRef};
use serde_json::Value as Json;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Mutex, MutexGuard};

/// One SQLite connection behind a mutex. SQLite serialises writers anyway;
/// for a typical JSON API this is plenty. Foreign keys are enforced and a
/// 5 s busy timeout is set; file databases use WAL.
pub struct SqliteConnection {
    inner: Mutex<rusqlite::Connection>,
}

impl SqliteConnection {
    pub fn open(path: &str) -> OrmResult<Self> {
        let conn = rusqlite::Connection::open(path).map_err(map_err)?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000; PRAGMA journal_mode = WAL;",
        )
        .map_err(map_err)?;
        Ok(Self {
            inner: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> OrmResult<Self> {
        let conn = rusqlite::Connection::open_in_memory().map_err(map_err)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(map_err)?;
        Ok(Self {
            inner: Mutex::new(conn),
        })
    }

    fn lock(&self) -> MutexGuard<'_, rusqlite::Connection> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn map_err(e: rusqlite::Error) -> OrmError {
    match &e {
        rusqlite::Error::SqliteFailure(err, msg)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            OrmError::Constraint(msg.clone().unwrap_or_else(|| e.to_string()))
        }
        _ => OrmError::Db(e.to_string()),
    }
}

fn to_raw(v: &SqlValue) -> SqlRaw {
    match v {
        SqlValue::Null => SqlRaw::Null,
        SqlValue::Bool(b) => SqlRaw::Integer(i64::from(*b)),
        SqlValue::Int(i) => SqlRaw::Integer(*i),
        SqlValue::Float(f) => SqlRaw::Real(*f),
        SqlValue::Text(s) => SqlRaw::Text(s.clone()),
    }
}

fn run_query(conn: &rusqlite::Connection, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
    observe(Dialect::Sqlite, sql, params, || {
        run_query_inner(conn, sql, params)
    })
}

fn run_query_inner(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[SqlValue],
) -> OrmResult<Vec<Row>> {
    let mut stmt = conn.prepare_cached(sql).map_err(map_err)?;
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let bound: Vec<SqlRaw> = params.iter().map(to_raw).collect();
    let mut rows = stmt
        .query(rusqlite::params_from_iter(bound.iter()))
        .map_err(map_err)?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(map_err)? {
        let mut map = Row::new();
        for (i, name) in names.iter().enumerate() {
            let v = match row.get_ref(i).map_err(map_err)? {
                ValueRef::Null => Json::Null,
                ValueRef::Integer(n) => Json::from(n),
                ValueRef::Real(f) => serde_json::Number::from_f64(f)
                    .map(Json::Number)
                    .unwrap_or(Json::Null),
                ValueRef::Text(t) => Json::String(String::from_utf8_lossy(t).into_owned()),
                // BLOBs come back as `\x<hex>` text, like PostgreSQL BYTEA.
                ValueRef::Blob(b) => Json::String(crate::value::hex_bytes(b)),
            };
            map.insert(name.clone(), v);
        }
        out.push(map);
    }
    Ok(out)
}

fn run_execute(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[SqlValue],
) -> OrmResult<ExecResult> {
    observe(Dialect::Sqlite, sql, params, || {
        run_execute_inner(conn, sql, params)
    })
}

fn run_execute_inner(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[SqlValue],
) -> OrmResult<ExecResult> {
    let bound: Vec<SqlRaw> = params.iter().map(to_raw).collect();
    let mut stmt = conn.prepare_cached(sql).map_err(map_err)?;
    let n = stmt
        .execute(rusqlite::params_from_iter(bound.iter()))
        .map_err(map_err)?;
    Ok(ExecResult {
        rows_affected: n as u64,
        last_insert_id: conn.last_insert_rowid(),
    })
}

impl Executor for SqliteConnection {
    fn query(&self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
        run_query(&self.lock(), sql, params)
    }
    fn execute(&self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult> {
        run_execute(&self.lock(), sql, params)
    }
    fn execute_batch(&self, sql: &str) -> OrmResult<()> {
        observe(Dialect::Sqlite, sql, &[], || {
            self.lock().execute_batch(sql).map_err(map_err)
        })
    }
}

/// The executor handed to a transaction closure: it borrows the already
/// locked connection, so it never tries to lock again.
struct Tx<'a>(&'a rusqlite::Connection);

impl Executor for Tx<'_> {
    fn query(&self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
        run_query(self.0, sql, params)
    }
    fn execute(&self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult> {
        run_execute(self.0, sql, params)
    }
    fn execute_batch(&self, sql: &str) -> OrmResult<()> {
        observe(Dialect::Sqlite, sql, &[], || {
            self.0.execute_batch(sql).map_err(map_err)
        })
    }
}

impl Connection for SqliteConnection {
    fn as_executor(&self) -> &dyn Executor {
        self
    }

    fn transaction(&self, f: &mut dyn FnMut(&dyn Executor) -> OrmResult<()>) -> OrmResult<()> {
        let guard = self.lock();
        guard.execute_batch("BEGIN IMMEDIATE").map_err(map_err)?;
        let tx = Tx(&guard);
        match catch_unwind(AssertUnwindSafe(|| f(&tx))) {
            Ok(Ok(())) => guard.execute_batch("COMMIT").map_err(map_err),
            Ok(Err(e)) => {
                let _ = guard.execute_batch("ROLLBACK");
                Err(e)
            }
            Err(panic) => {
                let _ = guard.execute_batch("ROLLBACK");
                resume_unwind(panic)
            }
        }
    }
}
