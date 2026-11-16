//! PostgreSQL driver (feature `postgres`).
//!
//! ```ignore
//! Db::set(PgConnection::connect("postgres://app:secret@localhost/app")?);
//! // or tune the pool:
//! Db::set(PgConnection::builder("postgres://...").max_connections(20).statement_timeout_ms(5_000).connect()?);
//! ```
//!
//! * A small built-in pool (default 10 connections, 10 s checkout timeout);
//!   broken connections are dropped and replaced transparently.
//! * The builder's `?` placeholders are rewritten to `$1, $2, ...` (never
//!   inside string literals, quoted identifiers or comments).
//! * Parameters are sent in PostgreSQL's *text* format and typed by the
//!   server from context, so `"42"` from a form compares fine with an
//!   `INTEGER` column, ISO strings go into `TIMESTAMPTZ`, JSON text into
//!   `JSONB`, UUID strings into `UUID`. Values are still bound parameters,
//!   never SQL.
//! * Results are decoded to JSON: integers, floats, `NUMERIC` (number when
//!   lossless, else string), booleans, text, `JSON/JSONB`, `UUID`,
//!   `TIMESTAMP[TZ]` (`YYYY-MM-DDTHH:MM:SSZ`), `DATE`, `TIME`, `BYTEA`
//!   (`\x..`), `INET`, enums and one-dimensional arrays.
//! * SQLSTATE class 23 (unique / FK / not-null / check) -> `Constraint`
//!   (409), class 22 (bad input such as `"abc"` for an integer) ->
//!   `Invalid` (400), everything else -> `Db` (500, logged).

use crate::conn::{Connection, Dialect, ExecResult, Executor, observe};
use crate::error::{OrmError, OrmResult};
use crate::time::format_unix;
use crate::value::{Row, SqlValue, hex_bytes};
use bytes::{BufMut, BytesMut};
use postgres::types::{FromSql, IsNull, Kind, ToSql, Type, to_sql_checked};
use postgres::{Client, Statement};
use serde_json::Value as Json;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

type BoxErr = Box<dyn StdError + Sync + Send>;

const STMT_CACHE: usize = 256;

// ----------------------------------------------------------------------
// public API
// ----------------------------------------------------------------------

/// A pooled PostgreSQL connection. Cheap to share: put it in `Db::set`.
pub struct PgConnection {
    config: postgres::Config,
    tls: Tls,
    max: usize,
    timeout: Duration,
    statement_timeout_ms: Option<u64>,
    state: Mutex<PoolState>,
    available: Condvar,
}

#[derive(Clone)]
enum Tls {
    None,
    #[cfg(feature = "postgres-tls")]
    Native(postgres_native_tls::MakeTlsConnector),
}

struct PoolState {
    idle: Vec<PgConn>,
    total: usize,
}

/// Configure a [`PgConnection`] before connecting.
pub struct PgBuilder {
    url: String,
    max: usize,
    timeout: Duration,
    statement_timeout_ms: Option<u64>,
}

impl PgBuilder {
    /// Maximum open connections (default 10).
    pub fn max_connections(mut self, n: usize) -> Self {
        self.max = n.max(1);
        self
    }

    /// How long a request waits for a free connection (default 10 s).
    pub fn checkout_timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// `SET statement_timeout` on every connection (kills runaway queries).
    pub fn statement_timeout_ms(mut self, ms: u64) -> Self {
        self.statement_timeout_ms = Some(ms);
        self
    }

    pub fn connect(self) -> OrmResult<PgConnection> {
        let config: postgres::Config = self
            .url
            .parse()
            .map_err(|e: postgres::Error| OrmError::Invalid(format!("bad PostgreSQL URL: {e}")))?;
        let tls = pick_tls(&config)?;
        let pool = PgConnection {
            config,
            tls,
            max: self.max,
            timeout: self.timeout,
            statement_timeout_ms: self.statement_timeout_ms,
            state: Mutex::new(PoolState {
                idle: Vec::new(),
                total: 0,
            }),
            available: Condvar::new(),
        };
        // fail fast on bad credentials / host
        let first = pool.open()?;
        {
            let mut st = pool.lock();
            st.total = 1;
            st.idle.push(first);
        }
        Ok(pool)
    }
}

#[cfg(feature = "postgres-tls")]
fn pick_tls(config: &postgres::Config) -> OrmResult<Tls> {
    use postgres::config::SslMode;
    let mut b = native_tls::TlsConnector::builder();
    match config.get_ssl_mode() {
        SslMode::Disable => return Ok(Tls::None),
        // `prefer` (the default): opportunistic encryption like libpq,
        // the certificate is not checked.
        SslMode::Prefer => {
            b.danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
        }
        // `require`: the server certificate must be valid for the host
        // (stricter than libpq's `require`, equal to `verify-full`).
        _ => {}
    }
    let connector = b
        .build()
        .map_err(|e| OrmError::Db(format!("TLS setup failed: {e}")))?;
    Ok(Tls::Native(postgres_native_tls::MakeTlsConnector::new(
        connector,
    )))
}

#[cfg(not(feature = "postgres-tls"))]
fn pick_tls(config: &postgres::Config) -> OrmResult<Tls> {
    use postgres::config::SslMode;
    if config.get_ssl_mode() == SslMode::Require {
        return Err(OrmError::Invalid(
            "sslmode=require needs the `postgres-tls` feature of fornthulani-plugin-orm".into(),
        ));
    }
    Ok(Tls::None)
}

impl PgConnection {
    /// Connect with defaults (10 connections). `url` is a libpq URL or
    /// key/value string: `postgres://user:pass@host:5432/db?sslmode=require`.
    pub fn connect(url: &str) -> OrmResult<Self> {
        Self::builder(url).connect()
    }

    pub fn builder(url: &str) -> PgBuilder {
        PgBuilder {
            url: url.to_string(),
            max: 10,
            timeout: Duration::from_secs(10),
            statement_timeout_ms: None,
        }
    }

    fn lock(&self) -> MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn open(&self) -> OrmResult<PgConn> {
        let mut client = match &self.tls {
            Tls::None => self.config.connect(postgres::NoTls),
            #[cfg(feature = "postgres-tls")]
            Tls::Native(t) => self.config.connect(t.clone()),
        }
        .map_err(|e| OrmError::Db(format!("PostgreSQL connect failed: {e}")))?;
        let mut init = String::from("SET TIME ZONE 'UTC';");
        if let Some(ms) = self.statement_timeout_ms {
            init.push_str(&format!("SET statement_timeout = {ms};"));
        }
        client.batch_execute(&init).map_err(map_err)?;
        Ok(PgConn {
            client,
            stmts: HashMap::new(),
        })
    }

    fn checkout(&self) -> OrmResult<Pooled<'_>> {
        let deadline = Instant::now() + self.timeout;
        let mut st = self.lock();
        loop {
            while let Some(c) = st.idle.pop() {
                if c.client.is_closed() {
                    st.total -= 1;
                    continue;
                }
                return Ok(Pooled {
                    pool: self,
                    conn: Some(c),
                    broken: false,
                });
            }
            if st.total < self.max {
                st.total += 1;
                drop(st);
                return match self.open() {
                    Ok(c) => Ok(Pooled {
                        pool: self,
                        conn: Some(c),
                        broken: false,
                    }),
                    Err(e) => {
                        self.lock().total -= 1;
                        self.available.notify_one();
                        Err(e)
                    }
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(OrmError::Db(
                    "timed out waiting for a PostgreSQL connection".into(),
                ));
            }
            st = self
                .available
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Open connections right now (idle + in use).
    pub fn connections(&self) -> usize {
        self.lock().total
    }
}

struct Pooled<'a> {
    pool: &'a PgConnection,
    conn: Option<PgConn>,
    broken: bool,
}

impl Pooled<'_> {
    fn get(&mut self) -> &mut PgConn {
        self.conn
            .as_mut()
            .expect("pooled connection present until drop")
    }
}

impl Drop for Pooled<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            let mut st = self.pool.lock();
            if self.broken || c.client.is_closed() {
                st.total -= 1;
            } else {
                st.idle.push(c);
            }
            drop(st);
            self.pool.available.notify_one();
        }
    }
}

// ----------------------------------------------------------------------
// one connection
// ----------------------------------------------------------------------

struct PgConn {
    client: Client,
    stmts: HashMap<String, Statement>,
}

impl PgConn {
    fn prepare(&mut self, sql: &str) -> Result<Statement, postgres::Error> {
        if let Some(s) = self.stmts.get(sql) {
            return Ok(s.clone());
        }
        let s = self.client.prepare(sql)?;
        if self.stmts.len() >= STMT_CACHE {
            self.stmts.clear();
        }
        self.stmts.insert(sql.to_string(), s.clone());
        Ok(s)
    }

    /// Run with one retry when a cached plan went stale after DDL.
    fn with_stmt<T>(
        &mut self,
        sql: &str,
        params: &[SqlValue],
        f: impl Fn(&mut Client, &Statement, &[&(dyn ToSql + Sync)]) -> Result<T, postgres::Error>,
    ) -> OrmResult<T> {
        let sql = rewrite_placeholders(sql);
        let wrapped: Vec<Param<'_>> = params.iter().map(Param).collect();
        let refs: Vec<&(dyn ToSql + Sync)> =
            wrapped.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
        for attempt in 0..2 {
            let stmt = self.prepare(&sql).map_err(map_err)?;
            if stmt.params().len() != params.len() {
                return Err(OrmError::Invalid(format!(
                    "statement expects {} parameters, got {}",
                    stmt.params().len(),
                    params.len()
                )));
            }
            match f(&mut self.client, &stmt, &refs) {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 && is_stale_plan(&e) => {
                    self.stmts.clear();
                }
                Err(e) => return Err(map_err(e)),
            }
        }
        Err(OrmError::Db("statement failed after re-prepare".into()))
    }

    fn query(&mut self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
        observe(Dialect::Postgres, sql, params, || {
            let (names, rows) = self.with_stmt(sql, params, |client, stmt, refs| {
                let names: Vec<String> = stmt
                    .columns()
                    .iter()
                    .map(|c| c.name().to_string())
                    .collect();
                Ok((names, client.query(stmt, refs)?))
            })?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let mut map = Row::new();
                for (i, name) in names.iter().enumerate() {
                    let Cell(v) = row
                        .try_get::<_, Cell>(i)
                        .map_err(|e| OrmError::Db(format!("cannot decode column `{name}`: {e}")))?;
                    map.insert(name.clone(), v);
                }
                out.push(map);
            }
            Ok(out)
        })
    }

    fn execute(&mut self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult> {
        observe(Dialect::Postgres, sql, params, || {
            let n = self.with_stmt(sql, params, |client, stmt, refs| client.execute(stmt, refs))?;
            Ok(ExecResult {
                rows_affected: n,
                last_insert_id: 0,
            })
        })
    }

    fn batch(&mut self, sql: &str) -> OrmResult<()> {
        observe(Dialect::Postgres, sql, &[], || {
            // DDL can invalidate cached plans
            self.stmts.clear();
            self.client.batch_execute(sql).map_err(map_err)
        })
    }
}

fn is_stale_plan(e: &postgres::Error) -> bool {
    e.as_db_error()
        .is_some_and(|d| d.code().code() == "0A000" && d.message().contains("cached plan"))
}

fn map_err(e: postgres::Error) -> OrmError {
    match e.as_db_error() {
        Some(db) => {
            let code = db.code().code();
            let msg = match db.detail() {
                Some(d) => format!("{code}: {} ({d})", db.message()),
                None => format!("{code}: {}", db.message()),
            };
            if code.starts_with("23") {
                OrmError::Constraint(msg)
            } else if code.starts_with("22") {
                OrmError::Invalid(msg)
            } else {
                OrmError::Db(msg)
            }
        }
        None => OrmError::Db(e.to_string()),
    }
}

// ----------------------------------------------------------------------
// Executor / Connection
// ----------------------------------------------------------------------

impl Executor for PgConnection {
    fn query(&self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
        let mut c = self.checkout()?;
        let r = c.get().query(sql, params);
        if c.get().client.is_closed() {
            c.broken = true;
        }
        r
    }
    fn execute(&self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult> {
        let mut c = self.checkout()?;
        c.get().execute(sql, params)
    }
    fn execute_batch(&self, sql: &str) -> OrmResult<()> {
        let mut c = self.checkout()?;
        c.get().batch(sql)
    }
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }
}

/// The executor inside a transaction: one pinned connection.
struct PgTx<'a>(RefCell<&'a mut PgConn>);

impl Executor for PgTx<'_> {
    fn query(&self, sql: &str, params: &[SqlValue]) -> OrmResult<Vec<Row>> {
        self.0.borrow_mut().query(sql, params)
    }
    fn execute(&self, sql: &str, params: &[SqlValue]) -> OrmResult<ExecResult> {
        self.0.borrow_mut().execute(sql, params)
    }
    fn execute_batch(&self, sql: &str) -> OrmResult<()> {
        self.0.borrow_mut().batch(sql)
    }
    fn dialect(&self) -> Dialect {
        Dialect::Postgres
    }
}

impl Connection for PgConnection {
    fn as_executor(&self) -> &dyn Executor {
        self
    }

    fn transaction(&self, f: &mut dyn FnMut(&dyn Executor) -> OrmResult<()>) -> OrmResult<()> {
        let mut pooled = self.checkout()?;
        pooled
            .get()
            .client
            .batch_execute("BEGIN")
            .map_err(map_err)?;
        let result = {
            let tx = PgTx(RefCell::new(pooled.get()));
            catch_unwind(AssertUnwindSafe(|| f(&tx)))
        };
        let conn = pooled.get();
        match result {
            Ok(Ok(())) => match conn.client.batch_execute("COMMIT") {
                Ok(()) => Ok(()),
                Err(e) => {
                    let _ = conn.client.batch_execute("ROLLBACK");
                    Err(map_err(e))
                }
            },
            Ok(Err(e)) => {
                if conn.client.batch_execute("ROLLBACK").is_err() {
                    pooled.broken = true;
                }
                Err(e)
            }
            Err(panic) => {
                if conn.client.batch_execute("ROLLBACK").is_err() {
                    pooled.broken = true;
                }
                drop(pooled);
                resume_unwind(panic)
            }
        }
    }
}

// ----------------------------------------------------------------------
// placeholders
// ----------------------------------------------------------------------

/// `?` -> `$1, $2, ...`, skipping string literals, quoted identifiers,
/// dollar-quoted strings and comments. SQL already written with `$n`
/// placeholders passes through unchanged.
pub(crate) fn rewrite_placeholders(sql: &str) -> Cow<'_, str> {
    if !sql.contains('?') {
        return Cow::Borrowed(sql);
    }
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    let mut i = 0;
    let mut start = 0;
    while i < b.len() {
        match b[i] {
            b'?' => {
                out.push_str(&sql[start..i]);
                n += 1;
                out.push_str(&format!("${n}"));
                i += 1;
                start = i;
            }
            q @ (b'\'' | b'"') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b'$' => {
                // $tag$ ... $tag$
                let rest = &sql[i + 1..];
                match rest.find('$') {
                    Some(end)
                        if rest[..end]
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                            && !rest[..end]
                                .bytes()
                                .next()
                                .is_some_and(|c| c.is_ascii_digit()) =>
                    {
                        let tag = &sql[i..i + end + 2];
                        let body_start = i + end + 2;
                        match sql[body_start..].find(tag) {
                            Some(p) => i = body_start + p + tag.len(),
                            None => i = b.len(),
                        }
                    }
                    _ => i += 1,
                }
            }
            _ => i += 1,
        }
    }
    out.push_str(&sql[start.min(sql.len())..]);
    Cow::Owned(out)
}

// ----------------------------------------------------------------------
// parameters: always text format, typed by the server
// ----------------------------------------------------------------------

#[derive(Debug)]
struct Param<'a>(&'a SqlValue);

fn is_numeric_type(ty: &Type) -> bool {
    matches!(
        *ty,
        Type::INT2
            | Type::INT4
            | Type::INT8
            | Type::OID
            | Type::NUMERIC
            | Type::FLOAT4
            | Type::FLOAT8
    )
}

fn param_text(v: &SqlValue, ty: &Type) -> Option<String> {
    Some(match v {
        SqlValue::Null => return None,
        SqlValue::Bool(b) if is_numeric_type(ty) => (if *b { "1" } else { "0" }).into(),
        SqlValue::Bool(b) => (if *b { "true" } else { "false" }).into(),
        SqlValue::Int(i) if *ty == Type::BOOL => (if *i != 0 { "true" } else { "false" }).into(),
        SqlValue::Int(i) => i.to_string(),
        SqlValue::Float(f) if f.is_nan() => "NaN".into(),
        SqlValue::Float(f) if f.is_infinite() => {
            (if *f > 0.0 { "Infinity" } else { "-Infinity" }).into()
        }
        SqlValue::Float(f) => f.to_string(),
        SqlValue::Text(s) => s.clone(),
    })
}

impl ToSql for Param<'_> {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, BoxErr> {
        match param_text(self.0, ty) {
            None => Ok(IsNull::Yes),
            Some(s) => {
                out.put_slice(s.as_bytes());
                Ok(IsNull::No)
            }
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _ty: &Type) -> postgres::types::Format {
        postgres::types::Format::Text
    }

    to_sql_checked!();
}

// ----------------------------------------------------------------------
// results: binary format -> JSON
// ----------------------------------------------------------------------

struct Cell(Json);

impl<'a> FromSql<'a> for Cell {
    fn from_sql(ty: &Type, raw: &'a [u8]) -> Result<Self, BoxErr> {
        Ok(Cell(decode(ty, raw)?))
    }
    fn from_sql_null(_: &Type) -> Result<Self, BoxErr> {
        Ok(Cell(Json::Null))
    }
    fn accepts(_: &Type) -> bool {
        true
    }
}

fn be<const N: usize>(raw: &[u8]) -> Result<[u8; N], BoxErr> {
    raw.get(..N)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| "short binary value".into())
}

fn num_f64(f: f64) -> Json {
    serde_json::Number::from_f64(f)
        .map(Json::Number)
        .unwrap_or(Json::Null)
}

/// Seconds between 1970-01-01 and PostgreSQL's epoch 2000-01-01.
const PG_EPOCH: i64 = 946_684_800;

fn timestamp(us: i64, tz: bool) -> Json {
    if us == i64::MAX {
        return Json::String("infinity".into());
    }
    if us == i64::MIN {
        return Json::String("-infinity".into());
    }
    let secs = us.div_euclid(1_000_000) + PG_EPOCH;
    let frac = us.rem_euclid(1_000_000);
    let mut s = format_unix(secs); // ...Z
    s.pop();
    if frac != 0 {
        let f = format!("{frac:06}");
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    if tz {
        s.push('Z');
    }
    Json::String(s)
}

fn decode(ty: &Type, raw: &[u8]) -> Result<Json, BoxErr> {
    Ok(match *ty {
        Type::BOOL => Json::Bool(raw.first().is_some_and(|b| *b != 0)),
        Type::INT2 => Json::from(i16::from_be_bytes(be(raw)?)),
        Type::INT4 => Json::from(i32::from_be_bytes(be(raw)?)),
        Type::INT8 => Json::from(i64::from_be_bytes(be(raw)?)),
        Type::OID => Json::from(u32::from_be_bytes(be(raw)?)),
        Type::FLOAT4 => num_f64(f64::from(f32::from_be_bytes(be(raw)?))),
        Type::FLOAT8 => num_f64(f64::from_be_bytes(be(raw)?)),
        Type::NUMERIC => numeric(raw)?,
        Type::JSON => serde_json::from_slice(raw)?,
        Type::JSONB => match raw.split_first() {
            Some((1, rest)) => serde_json::from_slice(rest)?,
            _ => return Err("unknown JSONB version".into()),
        },
        Type::UUID => {
            let b: [u8; 16] = be(raw)?;
            let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
            Json::String(format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            ))
        }
        Type::TIMESTAMPTZ => timestamp(i64::from_be_bytes(be(raw)?), true),
        Type::TIMESTAMP => timestamp(i64::from_be_bytes(be(raw)?), false),
        Type::DATE => {
            let d = i32::from_be_bytes(be(raw)?);
            match d {
                i32::MAX => Json::String("infinity".into()),
                i32::MIN => Json::String("-infinity".into()),
                _ => {
                    let s = format_unix((i64::from(d)) * 86_400 + PG_EPOCH);
                    Json::String(s[..10].to_string())
                }
            }
        }
        Type::TIME => {
            let us = i64::from_be_bytes(be(raw)?);
            let s = us / 1_000_000;
            let frac = us % 1_000_000;
            let mut out = format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60);
            if frac != 0 {
                out.push('.');
                out.push_str(format!("{frac:06}").trim_end_matches('0'));
            }
            Json::String(out)
        }
        Type::BYTEA => Json::String(hex_bytes(raw)),
        Type::INET | Type::CIDR => inet(raw)?,
        _ => match ty.kind() {
            Kind::Array(elem) => array(elem, raw)?,
            Kind::Domain(inner) => decode(inner, raw)?,
            // TEXT, VARCHAR, BPCHAR, NAME, CITEXT, enums, XML, ... are UTF-8 on the wire
            _ => match std::str::from_utf8(raw) {
                Ok(s) => Json::String(s.to_string()),
                Err(_) => {
                    return Err(format!(
                        "unsupported column type `{}` (cast it to text in SQL)",
                        ty.name()
                    )
                    .into());
                }
            },
        },
    })
}

/// PostgreSQL NUMERIC (base-10000 digits) -> exact decimal string ->
/// JSON number when that is lossless, else a string.
fn numeric(raw: &[u8]) -> Result<Json, BoxErr> {
    let ndigits = u16::from_be_bytes(be(raw)?) as usize;
    let weight = i16::from_be_bytes(be(&raw[2..])?) as i32;
    let sign = u16::from_be_bytes(be(&raw[4..])?);
    let dscale = u16::from_be_bytes(be(&raw[6..])?) as usize;
    match sign {
        0xC000 => return Ok(Json::String("NaN".into())),
        0xD000 => return Ok(Json::String("Infinity".into())),
        0xF000 => return Ok(Json::String("-Infinity".into())),
        _ => {}
    }
    let mut digits = Vec::with_capacity(ndigits);
    for k in 0..ndigits {
        digits.push(i16::from_be_bytes(be(raw
            .get(8 + 2 * k..)
            .ok_or("short NUMERIC")?)?));
    }
    let digit = |idx: i32| -> i16 {
        if idx >= 0 && (idx as usize) < digits.len() {
            digits[idx as usize]
        } else {
            0
        }
    };
    let mut s = String::new();
    if sign == 0x4000 {
        s.push('-');
    }
    if weight < 0 {
        s.push('0');
    } else {
        for i in 0..=weight {
            if i == 0 {
                s.push_str(&digit(i).to_string());
            } else {
                s.push_str(&format!("{:04}", digit(i)));
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::new();
        let mut k = 0;
        while frac.len() < dscale {
            frac.push_str(&format!("{:04}", digit(weight + 1 + k)));
            k += 1;
        }
        frac.truncate(dscale);
        s.push('.');
        s.push_str(&frac);
    }
    if s == "-0" || (s.starts_with("-0") && s[1..].chars().all(|c| c == '0' || c == '.')) {
        s.remove(0);
    }
    Ok(decimal_to_json(&s))
}

fn decimal_to_json(s: &str) -> Json {
    if !s.contains('.') {
        if let Ok(i) = s.parse::<i64>() {
            return Json::from(i);
        }
        return Json::String(s.to_string());
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    if !trimmed.contains('.')
        && let Ok(i) = trimmed.parse::<i64>()
    {
        return Json::from(i);
    }
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() && f.to_string() == trimmed => num_f64(f),
        _ => Json::String(s.to_string()),
    }
}

fn inet(raw: &[u8]) -> Result<Json, BoxErr> {
    let (family, bits, nb) = (
        raw.first().copied(),
        raw.get(1).copied(),
        raw.get(3).copied(),
    );
    let (Some(family), Some(bits), Some(nb)) = (family, bits, nb) else {
        return Err("short INET".into());
    };
    let addr = raw.get(4..4 + nb as usize).ok_or("short INET")?;
    let (text, full) = match family {
        2 => (
            addr.iter()
                .map(|b| b.to_string())
                .collect::<Vec<_>>()
                .join("."),
            32,
        ),
        _ => {
            let groups: Vec<String> = addr
                .chunks(2)
                .map(|c| format!("{:x}", u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)])))
                .collect();
            (groups.join(":"), 128)
        }
    };
    Ok(Json::String(if bits == full {
        text
    } else {
        format!("{text}/{bits}")
    }))
}

/// Binary array -> (nested) JSON array.
fn array(elem: &Type, raw: &[u8]) -> Result<Json, BoxErr> {
    let ndim = i32::from_be_bytes(be(raw)?) as usize;
    if ndim == 0 {
        return Ok(Json::Array(Vec::new()));
    }
    let mut pos = 12;
    let mut dims = Vec::with_capacity(ndim);
    for _ in 0..ndim {
        dims.push(i32::from_be_bytes(be(raw.get(pos..).ok_or("short array")?)?) as usize);
        pos += 8;
    }
    let total: usize = dims.iter().product();
    let mut items = Vec::with_capacity(total);
    for _ in 0..total {
        let len = i32::from_be_bytes(be(raw.get(pos..).ok_or("short array")?)?);
        pos += 4;
        if len < 0 {
            items.push(Json::Null);
        } else {
            let end = pos + len as usize;
            items.push(decode(elem, raw.get(pos..end).ok_or("short array")?)?);
            pos = end;
        }
    }
    // fold from the innermost dimension outwards
    for d in dims.iter().skip(1).rev() {
        items = items.chunks(*d).map(|c| Json::Array(c.to_vec())).collect();
    }
    Ok(Json::Array(items))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders() {
        assert_eq!(
            rewrite_placeholders("SELECT * FROM t WHERE a = ? AND b IN (?, ?)"),
            "SELECT * FROM t WHERE a = $1 AND b IN ($2, $3)"
        );
        assert_eq!(
            rewrite_placeholders("SELECT '?' , \"a?\", ? -- what?\n, ?"),
            "SELECT '?' , \"a?\", $1 -- what?\n, $2"
        );
        assert_eq!(
            rewrite_placeholders("SELECT 'it''s ?', ?"),
            "SELECT 'it''s ?', $1"
        );
        assert_eq!(
            rewrite_placeholders("SELECT /* ? */ ?"),
            "SELECT /* ? */ $1"
        );
        assert_eq!(
            rewrite_placeholders("SELECT $$ ? $$, ?"),
            "SELECT $$ ? $$, $1"
        );
        assert_eq!(rewrite_placeholders("SELECT $1"), "SELECT $1");
    }

    #[test]
    fn param_text_is_type_aware() {
        assert_eq!(param_text(&SqlValue::Bool(true), &Type::INT4).unwrap(), "1");
        assert_eq!(
            param_text(&SqlValue::Bool(false), &Type::BOOL).unwrap(),
            "false"
        );
        assert_eq!(param_text(&SqlValue::Int(1), &Type::BOOL).unwrap(), "true");
        assert_eq!(param_text(&SqlValue::Int(7), &Type::TEXT).unwrap(), "7");
        assert!(param_text(&SqlValue::Null, &Type::TEXT).is_none());
    }

    fn numeric_bytes(ndigits: u16, weight: i16, sign: u16, dscale: u16, digits: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend(ndigits.to_be_bytes());
        v.extend(weight.to_be_bytes());
        v.extend(sign.to_be_bytes());
        v.extend(dscale.to_be_bytes());
        for d in digits {
            v.extend(d.to_be_bytes());
        }
        v
    }

    #[test]
    fn numeric_decoding() {
        // 19.99
        assert_eq!(
            numeric(&numeric_bytes(2, 0, 0, 2, &[19, 9900])).unwrap(),
            Json::from(19.99)
        );
        // 12345678 (weight 1: 1234 5678)
        assert_eq!(
            numeric(&numeric_bytes(2, 1, 0, 0, &[1234, 5678])).unwrap(),
            Json::from(12_345_678)
        );
        // -0.05
        assert_eq!(
            numeric(&numeric_bytes(1, -1, 0x4000, 2, &[500])).unwrap(),
            Json::from(-0.05)
        );
        // 2.0000 -> 2
        assert_eq!(
            numeric(&numeric_bytes(1, 0, 0, 4, &[2])).unwrap(),
            Json::from(2)
        );
        // 0.00
        assert_eq!(
            numeric(&numeric_bytes(0, 0, 0, 2, &[])).unwrap(),
            Json::from(0)
        );
        // more precision than f64 -> exact string
        let big = numeric(&numeric_bytes(5, 0, 0, 16, &[1, 1234, 5678, 9012, 3456])).unwrap();
        assert_eq!(big, Json::String("1.1234567890123456".into()));
    }

    #[test]
    fn timestamps() {
        assert_eq!(
            timestamp(0, true),
            Json::String("2000-01-01T00:00:00Z".into())
        );
        assert_eq!(
            timestamp(1_500_000, false),
            Json::String("2000-01-01T00:00:01.5".into())
        );
        assert_eq!(
            timestamp(-1_000_000, true),
            Json::String("1999-12-31T23:59:59Z".into())
        );
    }
}
