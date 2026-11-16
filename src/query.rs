//! The query builder.
//!
//! Security model:
//!   * values are always bound parameters (`?`), never spliced into SQL;
//!   * table and column names are validated (`[A-Za-z_][A-Za-z0-9_]*`, at
//!     most `table.column`, optional `AS alias`) and double-quoted;
//!   * operators, sort directions and aggregate functions come from fixed
//!     whitelists;
//!   * the only ways to inject SQL text are `where_raw`, `select_raw`,
//!     `having_raw` and `order_by_raw`, which take a `&'static str`, so
//!     request data cannot reach them;
//!   * `update` / `delete` without a `WHERE` are refused unless you call
//!     `all_rows()`;
//!   * an empty `IN ()` matches nothing (never everything).
//!
//! Builder methods never fail while chaining: a bad identifier is
//! remembered and reported by the terminal call (`get`, `first`, ...).

use crate::conn::{Dialect, ExecResult, Executor, with_conn};
use crate::error::{OrmError, OrmResult};
use crate::value::{Row, SqlValue};
use serde_json::{Map, Value};

pub(crate) const MAX_IN_LIST: usize = 10_000;
/// Rows per statement for `insert_many` / `upsert` (keeps every database
/// under its bound-parameter limit).
const INSERT_CHUNK_PARAMS: usize = 30_000;

fn part_ok(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 64
        && p.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn bad_ident(s: &str) -> OrmError {
    let shown: String = s.chars().take(40).collect();
    OrmError::Invalid(format!("invalid identifier `{shown}`"))
}

/// Validate and double-quote `name`, `table.column`, `*` or `table.*`.
pub(crate) fn quote_ident(s: &str) -> OrmResult<String> {
    if s == "*" {
        return Ok("*".to_string());
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 2 {
        return Err(bad_ident(s));
    }
    let mut out = Vec::with_capacity(parts.len());
    for (i, p) in parts.iter().enumerate() {
        if *p == "*" && i == parts.len() - 1 && parts.len() == 2 {
            out.push("*".to_string());
        } else if part_ok(p) {
            out.push(format!("\"{p}\""));
        } else {
            return Err(bad_ident(s));
        }
    }
    Ok(out.join("."))
}

/// `col`, `t.col` or `t.col as alias` (case-insensitive `as`).
pub(crate) fn quote_select(s: &str) -> OrmResult<String> {
    let lower = s.to_ascii_lowercase();
    match lower.find(" as ") {
        Some(i) => {
            let (col, alias) = (s[..i].trim(), s[i + 4..].trim());
            if !part_ok(alias) {
                return Err(bad_ident(s));
            }
            Ok(format!("{} AS \"{alias}\"", quote_ident(col)?))
        }
        None => quote_ident(s.trim()),
    }
}

/// The key a column has in result rows (`t.col as x` -> `x`, `t.col` -> `col`).
pub(crate) fn result_key(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if let Some(i) = lower.find(" as ") {
        return s[i + 4..].trim().to_string();
    }
    s.rsplit('.').next().unwrap_or(s).trim().to_string()
}

fn norm_op(op: &str) -> Option<&'static str> {
    Some(match op.trim().to_ascii_lowercase().as_str() {
        "=" => "=",
        "!=" => "!=",
        "<>" => "<>",
        "<" => "<",
        "<=" => "<=",
        ">" => ">",
        ">=" => ">=",
        "like" => "LIKE",
        "not like" => "NOT LIKE",
        "ilike" => "ILIKE",
        "not ilike" => "NOT ILIKE",
        _ => return None,
    })
}

/// `ILIKE` is PostgreSQL-only; SQLite's `LIKE` is already case-insensitive
/// for ASCII, so it is the closest equivalent.
fn op_sql(op: &'static str, d: Dialect) -> &'static str {
    match (op, d) {
        ("ILIKE", Dialect::Sqlite) => "LIKE",
        ("NOT ILIKE", Dialect::Sqlite) => "NOT LIKE",
        _ => op,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Joiner {
    And,
    Or,
}

#[derive(Clone, Debug)]
pub(crate) enum Cond {
    Cmp {
        col: String,
        op: &'static str,
        val: SqlValue,
    },
    Column {
        a: String,
        op: &'static str,
        b: String,
    },
    In {
        col: String,
        vals: Vec<SqlValue>,
        negate: bool,
    },
    InSub {
        col: String,
        sub: Box<Query>,
        negate: bool,
    },
    Exists {
        sub: Box<Query>,
        negate: bool,
    },
    Null {
        col: String,
        negate: bool,
    },
    Between {
        col: String,
        lo: SqlValue,
        hi: SqlValue,
        negate: bool,
    },
    Date {
        col: String,
        op: &'static str,
        val: SqlValue,
    },
    Json {
        col: String,
        path: Vec<String>,
        op: &'static str,
        val: SqlValue,
    },
    Raw {
        sql: &'static str,
        params: Vec<SqlValue>,
    },
    Group(Vec<(Joiner, Cond)>),
}

/// Which soft-deleted rows a query sees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trashed {
    Without,
    With,
    Only,
}

/// Whitelisted aggregate functions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl Agg {
    fn sql(self) -> &'static str {
        match self {
            Agg::Count => "COUNT",
            Agg::Sum => "SUM",
            Agg::Avg => "AVG",
            Agg::Min => "MIN",
            Agg::Max => "MAX",
        }
    }
    pub(crate) fn name(self) -> &'static str {
        match self {
            Agg::Count => "count",
            Agg::Sum => "sum",
            Agg::Avg => "avg",
            Agg::Min => "min",
            Agg::Max => "max",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JoinKind {
    Inner,
    Left,
}

#[derive(Clone, Debug)]
struct Join {
    kind: JoinKind,
    table: String,
    a: String,
    op: &'static str,
    b: String,
}

#[derive(Clone, Debug)]
enum Order {
    Col(String, bool),
    Raw(&'static str),
}

/// What a sub-select in the column list computes.
#[derive(Clone, Debug)]
pub(crate) enum SubSelect {
    Agg(Agg, Option<String>),
    Exists,
}

/// How the SELECT is shaped (rows, or a single aggregate / existence probe
/// used inside another query).
#[derive(Clone, Debug)]
enum Mode {
    Rows,
    One,
    Agg(Agg, Option<String>),
}

/// One `SET` item of an UPDATE.
#[derive(Clone, Debug)]
enum Set {
    Value(String, SqlValue),
    Add(String, SqlValue),
}

#[derive(Clone, Debug)]
pub struct Query {
    table: String,
    alias: Option<String>,
    columns: Vec<String>,
    raw_selects: Vec<(&'static str, Vec<SqlValue>)>,
    sub_selects: Vec<(Query, SubSelect, String)>,
    distinct: bool,
    joins: Vec<Join>,
    pub(crate) wheres: Vec<(Joiner, Cond)>,
    groups: Vec<String>,
    havings: Vec<(&'static str, Vec<SqlValue>)>,
    orders: Vec<Order>,
    limit: Option<u64>,
    offset: Option<u64>,
    lock: Option<&'static str>,
    soft_col: Option<&'static str>,
    trashed: Trashed,
    all_rows: bool,
    pub(crate) conn: Option<String>,
    error: Option<OrmError>,
}

impl Query {
    pub fn new(table: &str) -> Self {
        Query {
            table: table.to_string(),
            alias: None,
            columns: Vec::new(),
            raw_selects: Vec::new(),
            sub_selects: Vec::new(),
            distinct: false,
            joins: Vec::new(),
            wheres: Vec::new(),
            groups: Vec::new(),
            havings: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
            lock: None,
            soft_col: None,
            trashed: Trashed::Without,
            all_rows: false,
            conn: None,
            error: None,
        }
    }

    /// Run on a named connection (see `Db::add_connection`).
    pub fn on(mut self, connection: &str) -> Self {
        self.conn = Some(connection.to_string());
        self
    }

    pub fn table_name(&self) -> &str {
        &self.table
    }

    /// The name other clauses use to refer to this table (`alias` or the
    /// table name without schema).
    pub(crate) fn ref_name(&self) -> String {
        match &self.alias {
            Some(a) => a.clone(),
            None => self
                .table
                .rsplit('.')
                .next()
                .unwrap_or(&self.table)
                .to_string(),
        }
    }

    /// `FROM "table" AS "alias"` (self-referencing relations need this).
    pub fn alias(mut self, alias: &str) -> Self {
        if part_ok(alias) {
            self.alias = Some(alias.to_string());
        } else {
            self.fail(bad_ident(alias));
        }
        self
    }

    pub(crate) fn soft_deletes(mut self, column: &'static str) -> Self {
        self.soft_col = Some(column);
        self
    }

    pub(crate) fn fail(&mut self, e: OrmError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    pub(crate) fn has_orders(&self) -> bool {
        !self.orders.is_empty()
    }

    fn ident_ok(&mut self, col: &str) -> bool {
        match quote_ident(col) {
            Ok(_) => true,
            Err(e) => {
                self.fail(e);
                false
            }
        }
    }

    // ---------- conditional building / scopes ----------

    /// `q.when(filter.is_some(), |q| q.where_eq("status", filter))`.
    pub fn when(self, cond: bool, f: impl FnOnce(Self) -> Self) -> Self {
        if cond { f(self) } else { self }
    }

    /// Apply a reusable scope: `q.scope(published)` with
    /// `fn published(q: Query) -> Query`.
    pub fn scope(self, f: impl FnOnce(Self) -> Self) -> Self {
        f(self)
    }

    /// Wrap every condition so far in one parenthesised group.
    pub(crate) fn grouped(mut self) -> Self {
        if self.wheres.len() > 1 {
            let all = std::mem::take(&mut self.wheres);
            self.wheres.push((Joiner::And, Cond::Group(all)));
        }
        self
    }

    /// Apply a (global) scope so that its conditions and the user's
    /// conditions are each parenthesised: `(user) AND (scope)`. An
    /// `or_where` on either side can never widen the other.
    pub(crate) fn with_scope(mut self, f: fn(Query) -> Query) -> Self {
        let user = std::mem::take(&mut self.wheres);
        let mut scoped = f(self);
        let scope_wheres = std::mem::take(&mut scoped.wheres);
        if !user.is_empty() {
            scoped.wheres.push((Joiner::And, Cond::Group(user)));
        }
        if !scope_wheres.is_empty() {
            scoped.wheres.push((Joiner::And, Cond::Group(scope_wheres)));
        }
        scoped
    }

    // ---------- selecting ----------

    /// Restrict the selected columns (default `*`). Accepts `col`,
    /// `table.col`, `table.*` and `col as alias`.
    pub fn select<I, S>(mut self, cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for c in cols {
            match quote_select(c.as_ref()) {
                Ok(_) => self.columns.push(c.as_ref().to_string()),
                Err(e) => self.fail(e),
            }
        }
        self
    }

    /// Add a raw select expression: `select_raw("COUNT(*) AS n", vec![])`.
    pub fn select_raw(mut self, sql: &'static str, params: Vec<SqlValue>) -> Self {
        self.raw_selects.push((sql, params));
        self
    }

    pub(crate) fn select_sub(mut self, sub: Query, what: SubSelect, alias: String) -> Self {
        if !part_ok(&alias) {
            self.fail(bad_ident(&alias));
            return self;
        }
        if let Some(e) = &sub.error {
            self.fail(e.clone());
            return self;
        }
        self.sub_selects.push((sub, what, alias));
        self
    }

    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    // ---------- joins ----------

    fn add_join(mut self, kind: JoinKind, table: &str, a: &str, op: &str, b: &str) -> Self {
        let Some(op) = norm_op(op).filter(|o| !o.contains("LIKE")) else {
            self.fail(OrmError::Invalid("unsupported join operator".into()));
            return self;
        };
        if self.ident_ok(table) && self.ident_ok(a) && self.ident_ok(b) {
            self.joins.push(Join {
                kind,
                table: table.to_string(),
                a: a.to_string(),
                op,
                b: b.to_string(),
            });
        }
        self
    }

    /// `INNER JOIN table ON a op b` (both sides are columns).
    pub fn join(self, table: &str, a: &str, op: &str, b: &str) -> Self {
        self.add_join(JoinKind::Inner, table, a, op, b)
    }

    pub fn left_join(self, table: &str, a: &str, op: &str, b: &str) -> Self {
        self.add_join(JoinKind::Left, table, a, op, b)
    }

    // ---------- where ----------

    fn push(mut self, j: Joiner, c: Cond) -> Self {
        self.wheres.push((j, c));
        self
    }

    fn add_cmp(mut self, j: Joiner, col: &str, op: &str, val: SqlValue) -> Self {
        let Some(op) = norm_op(op) else {
            self.fail(OrmError::Invalid("unsupported operator".into()));
            return self;
        };
        if !self.ident_ok(col) {
            return self;
        }
        let is_null = matches!(val, SqlValue::Null);
        let cond = match (is_null, op) {
            (true, "=") => Cond::Null {
                col: col.to_string(),
                negate: false,
            },
            (true, "!=") | (true, "<>") => Cond::Null {
                col: col.to_string(),
                negate: true,
            },
            _ => Cond::Cmp {
                col: col.to_string(),
                op,
                val,
            },
        };
        self.push(j, cond)
    }

    /// `WHERE col <op> ?`. Operators: `= != <> < <= > >= like "not like"
    /// ilike "not ilike"`. Comparing with `NULL` becomes `IS [NOT] NULL`.
    pub fn where_(self, col: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::And, col, op, val.into())
    }

    pub fn where_eq(self, col: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::And, col, "=", val.into())
    }

    pub fn or_where(self, col: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::Or, col, op, val.into())
    }

    pub fn or_where_eq(self, col: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::Or, col, "=", val.into())
    }

    /// `WHERE col = ?` for every key of a JSON object (`{"a": 1, "b": "x"}`).
    pub fn where_all(mut self, pairs: &Value) -> Self {
        match pairs {
            Value::Object(m) => {
                for (k, v) in m {
                    self = self.where_eq(k, SqlValue::from_json(v));
                }
                self
            }
            _ => {
                self.fail(OrmError::Invalid("expected a JSON object".into()));
                self
            }
        }
    }

    /// Case-insensitive `LIKE` on both databases (`ILIKE` on PostgreSQL).
    pub fn where_like(self, col: &str, pattern: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::And, col, "ilike", pattern.into())
    }

    /// Compare two columns: `where_column("updated_at", ">", "created_at")`.
    pub fn where_column(mut self, a: &str, op: &str, b: &str) -> Self {
        let Some(op) = norm_op(op).filter(|o| !o.contains("LIKE")) else {
            self.fail(OrmError::Invalid("unsupported operator".into()));
            return self;
        };
        if self.ident_ok(a) && self.ident_ok(b) {
            let c = Cond::Column {
                a: a.to_string(),
                op,
                b: b.to_string(),
            };
            return self.push(Joiner::And, c);
        }
        self
    }

    fn add_in<I, V>(mut self, j: Joiner, col: &str, vals: I, negate: bool) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        if !self.ident_ok(col) {
            return self;
        }
        let vals: Vec<SqlValue> = vals.into_iter().map(Into::into).collect();
        if vals.len() > MAX_IN_LIST {
            self.fail(OrmError::Invalid("IN list too long".into()));
            return self;
        }
        let c = Cond::In {
            col: col.to_string(),
            vals,
            negate,
        };
        self.push(j, c)
    }

    pub fn where_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.add_in(Joiner::And, col, vals, false)
    }

    pub fn or_where_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.add_in(Joiner::Or, col, vals, false)
    }

    pub fn where_not_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.add_in(Joiner::And, col, vals, true)
    }

    fn add_in_sub(mut self, col: &str, sub: Query, negate: bool) -> Self {
        if let Some(e) = &sub.error {
            self.fail(e.clone());
            return self;
        }
        if self.ident_ok(col) {
            let c = Cond::InSub {
                col: col.to_string(),
                sub: Box::new(sub),
                negate,
            };
            return self.push(Joiner::And, c);
        }
        self
    }

    /// `col IN (SELECT ...)`: `where_in_sub("id", Db::table("orders").select(["user_id"]))`.
    pub fn where_in_sub(self, col: &str, sub: Query) -> Self {
        self.add_in_sub(col, sub, false)
    }

    pub fn where_not_in_sub(self, col: &str, sub: Query) -> Self {
        self.add_in_sub(col, sub, true)
    }

    pub(crate) fn add_exists(mut self, j: Joiner, sub: Query, negate: bool) -> Self {
        if let Some(e) = &sub.error {
            self.fail(e.clone());
            return self;
        }
        self.push(
            j,
            Cond::Exists {
                sub: Box::new(sub),
                negate,
            },
        )
    }

    /// `EXISTS (SELECT 1 FROM ...)`. Correlate with `where_column`.
    pub fn where_exists(self, sub: Query) -> Self {
        self.add_exists(Joiner::And, sub, false)
    }

    pub fn where_not_exists(self, sub: Query) -> Self {
        self.add_exists(Joiner::And, sub, true)
    }

    fn add_null(mut self, j: Joiner, col: &str, negate: bool) -> Self {
        if self.ident_ok(col) {
            let c = Cond::Null {
                col: col.to_string(),
                negate,
            };
            return self.push(j, c);
        }
        self
    }

    pub fn where_null(self, col: &str) -> Self {
        self.add_null(Joiner::And, col, false)
    }
    pub fn where_not_null(self, col: &str) -> Self {
        self.add_null(Joiner::And, col, true)
    }
    pub fn or_where_null(self, col: &str) -> Self {
        self.add_null(Joiner::Or, col, false)
    }
    pub fn or_where_not_null(self, col: &str) -> Self {
        self.add_null(Joiner::Or, col, true)
    }

    fn add_between(mut self, col: &str, lo: SqlValue, hi: SqlValue, negate: bool) -> Self {
        if self.ident_ok(col) {
            let c = Cond::Between {
                col: col.to_string(),
                lo,
                hi,
                negate,
            };
            return self.push(Joiner::And, c);
        }
        self
    }

    pub fn where_between(
        self,
        col: &str,
        lo: impl Into<SqlValue>,
        hi: impl Into<SqlValue>,
    ) -> Self {
        self.add_between(col, lo.into(), hi.into(), false)
    }

    pub fn where_not_between(
        self,
        col: &str,
        lo: impl Into<SqlValue>,
        hi: impl Into<SqlValue>,
    ) -> Self {
        self.add_between(col, lo.into(), hi.into(), true)
    }

    /// Compare the date part of a timestamp column with `YYYY-MM-DD`.
    pub fn where_date(mut self, col: &str, op: &str, date: impl Into<SqlValue>) -> Self {
        let Some(op) = norm_op(op).filter(|o| !o.contains("LIKE")) else {
            self.fail(OrmError::Invalid("unsupported operator".into()));
            return self;
        };
        if self.ident_ok(col) {
            let c = Cond::Date {
                col: col.to_string(),
                op,
                val: date.into(),
            };
            return self.push(Joiner::And, c);
        }
        self
    }

    /// Compare a value inside a JSON column: `where_json("settings->theme", "=", "dark")`.
    /// Path segments are validated identifiers (or array indexes).
    pub fn where_json(mut self, path: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        let Some(op) = norm_op(op) else {
            self.fail(OrmError::Invalid("unsupported operator".into()));
            return self;
        };
        let mut parts = path.split("->").map(str::trim);
        let col = parts.next().unwrap_or_default().to_string();
        let segs: Vec<String> = parts.map(str::to_string).collect();
        let segs_ok = !segs.is_empty()
            && segs.iter().all(|s| {
                part_ok(s)
                    || (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit()))
            });
        if !segs_ok {
            self.fail(bad_ident(path));
            return self;
        }
        if self.ident_ok(&col) {
            let c = Cond::Json {
                col,
                path: segs,
                op,
                val: val.into(),
            };
            return self.push(Joiner::And, c);
        }
        self
    }

    /// A raw SQL condition with `?` placeholders. The SQL must be a string
    /// literal in your source (`&'static str`), which makes it impossible
    /// to pass request data as SQL text by accident.
    pub fn where_raw(self, sql: &'static str, params: Vec<SqlValue>) -> Self {
        self.push(Joiner::And, Cond::Raw { sql, params })
    }

    pub fn or_where_raw(self, sql: &'static str, params: Vec<SqlValue>) -> Self {
        self.push(Joiner::Or, Cond::Raw { sql, params })
    }

    fn add_group(mut self, j: Joiner, f: impl FnOnce(Query) -> Query) -> Self {
        let inner = f(Query::new("_group"));
        if let Some(e) = inner.error {
            self.fail(e);
            return self;
        }
        if !inner.wheres.is_empty() {
            self.wheres.push((j, Cond::Group(inner.wheres)));
        }
        self
    }

    /// Parenthesised conditions:
    /// `q.where_eq("a", 1).where_group(|g| g.where_eq("b", 2).or_where("c", "=", 3))`
    /// -> `a = ? AND (b = ? OR c = ?)`.
    pub fn where_group(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.add_group(Joiner::And, f)
    }

    pub fn or_where_group(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.add_group(Joiner::Or, f)
    }

    // ---------- grouping ----------

    pub fn group_by<I, S>(mut self, cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for c in cols {
            if self.ident_ok(c.as_ref()) {
                self.groups.push(c.as_ref().to_string());
            }
        }
        self
    }

    /// `HAVING <literal SQL>` (aggregate aliases are not portable, so write
    /// the aggregate: `having_raw("COUNT(*) > ?", vec![2.into()])`).
    pub fn having_raw(mut self, sql: &'static str, params: Vec<SqlValue>) -> Self {
        self.havings.push((sql, params));
        self
    }

    // ---------- ordering / paging ----------

    /// `direction` is `"asc"` or `"desc"`.
    pub fn order_by(mut self, col: &str, direction: &str) -> Self {
        let desc = match direction.trim().to_ascii_lowercase().as_str() {
            "asc" => false,
            "desc" => true,
            _ => {
                self.fail(OrmError::Invalid(
                    "order direction must be asc or desc".into(),
                ));
                return self;
            }
        };
        if self.ident_ok(col) {
            self.orders.push(Order::Col(col.to_string(), desc));
        }
        self
    }

    pub fn order_by_desc(self, col: &str) -> Self {
        self.order_by(col, "desc")
    }

    pub fn order_by_raw(mut self, sql: &'static str) -> Self {
        self.orders.push(Order::Raw(sql));
        self
    }

    /// Newest first by `created_at`.
    pub fn latest(self) -> Self {
        self.order_by("created_at", "desc")
    }

    /// Oldest first by `created_at`.
    pub fn oldest(self) -> Self {
        self.order_by("created_at", "asc")
    }

    /// Drop every `ORDER BY` added so far.
    pub fn reorder(mut self) -> Self {
        self.orders.clear();
        self
    }

    /// Sort by a client-supplied value, safely. `requested` is e.g. the
    /// `?sort=` parameter: `"name"` (ascending) or `"-name"` (descending).
    /// Anything not in `allowed` makes the query fail with a 400.
    pub fn sort_by(
        mut self,
        requested: Option<&str>,
        allowed: &[&str],
        default: (&str, &str),
    ) -> Self {
        match requested {
            None | Some("") => self.order_by(default.0, default.1),
            Some(r) => {
                let (col, dir) = match r.strip_prefix('-') {
                    Some(c) => (c, "desc"),
                    None => (r, "asc"),
                };
                if allowed.contains(&col) {
                    self.order_by(col, dir)
                } else {
                    self.fail(OrmError::Invalid("sort column not allowed".into()));
                    self
                }
            }
        }
    }

    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    pub fn take(self, n: u64) -> Self {
        self.limit(n)
    }

    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n);
        self
    }

    pub fn skip(self, n: u64) -> Self {
        self.offset(n)
    }

    /// `SELECT ... FOR UPDATE` on PostgreSQL (ignored on SQLite, whose
    /// transactions already lock the database). Use inside a transaction.
    pub fn lock_for_update(mut self) -> Self {
        self.lock = Some("FOR UPDATE");
        self
    }

    /// `FOR SHARE` on PostgreSQL (ignored on SQLite).
    pub fn shared_lock(mut self) -> Self {
        self.lock = Some("FOR SHARE");
        self
    }

    // ---------- soft deletes / safety switches ----------

    pub fn with_trashed(mut self) -> Self {
        self.trashed = Trashed::With;
        self
    }

    pub fn only_trashed(mut self) -> Self {
        self.trashed = Trashed::Only;
        self
    }

    /// Confirm that an `update` / `delete` with no conditions is intended.
    pub fn all_rows(mut self) -> Self {
        self.all_rows = true;
        self
    }

    // ---------- SQL generation ----------

    fn check(&self) -> OrmResult<()> {
        match &self.error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn table_clause(&self) -> OrmResult<String> {
        let t = quote_ident(&self.table)?;
        Ok(match &self.alias {
            Some(a) => format!("{t} AS \"{a}\""),
            None => t,
        })
    }

    fn soft_sql(&self, qualify: bool) -> OrmResult<Option<String>> {
        let Some(c) = self.soft_col else {
            return Ok(None);
        };
        let col = if qualify {
            quote_ident(&format!("{}.{c}", self.ref_name()))?
        } else {
            quote_ident(c)?
        };
        Ok(match self.trashed {
            Trashed::Without => Some(format!("{col} IS NULL")),
            Trashed::Only => Some(format!("{col} IS NOT NULL")),
            Trashed::With => None,
        })
    }

    fn where_sql(
        &self,
        d: Dialect,
        params: &mut Vec<SqlValue>,
        qualify: bool,
    ) -> OrmResult<String> {
        let user = compile_conds(&self.wheres, d, params)?;
        let soft = self.soft_sql(qualify || !self.joins.is_empty())?;
        Ok(match (user.is_empty(), soft) {
            (true, None) => String::new(),
            (true, Some(s)) => format!(" WHERE {s}"),
            (false, None) => format!(" WHERE {user}"),
            (false, Some(s)) => format!(" WHERE ({user}) AND {s}"),
        })
    }

    fn joins_sql(&self) -> OrmResult<String> {
        let mut s = String::new();
        for j in &self.joins {
            s.push_str(&format!(
                " {} JOIN {} ON {} {} {}",
                match j.kind {
                    JoinKind::Inner => "INNER",
                    JoinKind::Left => "LEFT",
                },
                quote_ident(&j.table)?,
                quote_ident(&j.a)?,
                j.op,
                quote_ident(&j.b)?
            ));
        }
        Ok(s)
    }

    fn columns_sql(&self, d: Dialect, params: &mut Vec<SqlValue>) -> OrmResult<String> {
        let mut parts = Vec::new();
        for c in &self.columns {
            parts.push(quote_select(c)?);
        }
        for (sql, p) in &self.raw_selects {
            parts.push((*sql).to_string());
            params.extend(p.iter().cloned());
        }
        if parts.is_empty() {
            parts.push(if self.sub_selects.is_empty() {
                "*".to_string()
            } else {
                format!("{}.*", quote_ident(&self.ref_name())?)
            });
        }
        for (sub, what, alias) in &self.sub_selects {
            let mode = match what {
                SubSelect::Agg(a, c) => Mode::Agg(*a, c.clone()),
                SubSelect::Exists => Mode::One,
            };
            let inner = sub.compile(d, params, &mode, true)?;
            parts.push(match what {
                SubSelect::Exists => format!("EXISTS ({inner}) AS \"{alias}\""),
                _ => format!("({inner}) AS \"{alias}\""),
            });
        }
        Ok(parts.join(", "))
    }

    fn compile(
        &self,
        d: Dialect,
        params: &mut Vec<SqlValue>,
        mode: &Mode,
        nested: bool,
    ) -> OrmResult<String> {
        self.check()?;
        let cols = match mode {
            Mode::Rows => self.columns_sql(d, params)?,
            Mode::One => "1".to_string(),
            Mode::Agg(a, col) => match col {
                Some(c) => format!("{}({})", a.sql(), quote_ident(c)?),
                None => format!("{}(*)", a.sql()),
            },
        };
        let mut sql = format!(
            "SELECT {}{cols} FROM {}",
            if self.distinct && matches!(mode, Mode::Rows) {
                "DISTINCT "
            } else {
                ""
            },
            self.table_clause()?
        );
        sql.push_str(&self.joins_sql()?);
        sql.push_str(&self.where_sql(d, params, nested)?);
        if !self.groups.is_empty() {
            let g = self
                .groups
                .iter()
                .map(|c| quote_ident(c))
                .collect::<OrmResult<Vec<_>>>()?;
            sql.push_str(&format!(" GROUP BY {}", g.join(", ")));
        }
        if !self.havings.is_empty() {
            let mut hs = Vec::new();
            for (h, p) in &self.havings {
                hs.push(format!("({h})"));
                params.extend(p.iter().cloned());
            }
            sql.push_str(&format!(" HAVING {}", hs.join(" AND ")));
        }
        if !matches!(mode, Mode::Rows) {
            return Ok(sql);
        }
        if !self.orders.is_empty() {
            let parts = self
                .orders
                .iter()
                .map(|o| match o {
                    Order::Col(c, desc) => quote_ident(c)
                        .map(|q| format!("{q} {}", if *desc { "DESC" } else { "ASC" })),
                    Order::Raw(r) => Ok((*r).to_string()),
                })
                .collect::<OrmResult<Vec<_>>>()?;
            sql.push_str(&format!(" ORDER BY {}", parts.join(", ")));
        }
        let cap = |n: u64| n.min(i64::MAX as u64);
        match (self.limit, self.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(" LIMIT {} OFFSET {}", cap(l), cap(o))),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {}", cap(l))),
            (None, Some(o)) => sql.push_str(&format!(" LIMIT {} OFFSET {}", i64::MAX, cap(o))),
            (None, None) => {}
        }
        if let (Some(lock), Dialect::Postgres) = (self.lock, d) {
            sql.push(' ');
            sql.push_str(lock);
        }
        Ok(sql)
    }

    /// SELECT for SQLite (`?` placeholders). See [`Query::to_select_sql_for`].
    pub fn to_select_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.to_select_sql_for(Dialect::Sqlite)
    }

    pub fn to_select_sql_for(&self, d: Dialect) -> OrmResult<(String, Vec<SqlValue>)> {
        let mut params = Vec::new();
        let sql = self.compile(d, &mut params, &Mode::Rows, false)?;
        Ok((sql, params))
    }

    pub fn to_count_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.to_count_sql_for(Dialect::Sqlite)
    }

    pub fn to_count_sql_for(&self, d: Dialect) -> OrmResult<(String, Vec<SqlValue>)> {
        let mut params = Vec::new();
        if self.distinct || !self.groups.is_empty() {
            // count the groups / distinct rows, not the underlying rows
            let mut inner = self.clone().reorder();
            inner.limit = None;
            inner.offset = None;
            inner.lock = None;
            if !inner.groups.is_empty() {
                // one row per group; selecting `*` is invalid with GROUP BY on PostgreSQL
                inner.columns = inner.groups.clone();
                inner.raw_selects.clear();
                inner.sub_selects.clear();
            }
            let s = inner.compile(d, &mut params, &Mode::Rows, false)?;
            return Ok((
                format!("SELECT COUNT(*) AS \"aggregate\" FROM ({s}) AS \"_counted\""),
                params,
            ));
        }
        let s = self.compile(d, &mut params, &Mode::Agg(Agg::Count, None), false)?;
        Ok((
            s.replacen("COUNT(*)", "COUNT(*) AS \"aggregate\"", 1),
            params,
        ))
    }

    fn to_aggregate_sql(
        &self,
        d: Dialect,
        agg: Agg,
        col: &str,
    ) -> OrmResult<(String, Vec<SqlValue>)> {
        let mut params = Vec::new();
        let s = self.compile(
            d,
            &mut params,
            &Mode::Agg(agg, Some(col.to_string())),
            false,
        )?;
        let head = format!("SELECT {}({})", agg.sql(), quote_ident(col)?);
        Ok((
            s.replacen(&head, &format!("{head} AS \"aggregate\""), 1),
            params,
        ))
    }

    fn guard_bulk(&self, what: &str) -> OrmResult<()> {
        self.check()?;
        if self.wheres.is_empty() && !self.all_rows {
            return Err(OrmError::Invalid(format!(
                "refusing to {what} every row; add a condition or call all_rows()"
            )));
        }
        if !self.joins.is_empty() || !self.groups.is_empty() {
            return Err(OrmError::Invalid(format!(
                "{what} does not support joins or GROUP BY"
            )));
        }
        Ok(())
    }

    fn update_sql(&self, d: Dialect, set: &[Set]) -> OrmResult<(String, Vec<SqlValue>)> {
        self.guard_bulk("UPDATE")?;
        if set.is_empty() {
            return Err(OrmError::Invalid("nothing to update".into()));
        }
        let mut params = Vec::new();
        let mut sets = Vec::new();
        for s in set {
            match s {
                Set::Value(c, v) => {
                    sets.push(format!("{} = ?", quote_ident(c)?));
                    params.push(v.clone());
                }
                Set::Add(c, v) => {
                    let q = quote_ident(c)?;
                    sets.push(format!("{q} = {q} + ?"));
                    params.push(v.clone());
                }
            }
        }
        let mut sql = format!("UPDATE {} SET {}", self.table_clause()?, sets.join(", "));
        sql.push_str(&self.where_sql(d, &mut params, false)?);
        Ok((sql, params))
    }

    pub fn to_update_sql(&self, set: &[(String, SqlValue)]) -> OrmResult<(String, Vec<SqlValue>)> {
        let items: Vec<Set> = set
            .iter()
            .map(|(c, v)| Set::Value(c.clone(), v.clone()))
            .collect();
        self.update_sql(Dialect::Sqlite, &items)
    }

    fn delete_sql(&self, d: Dialect) -> OrmResult<(String, Vec<SqlValue>)> {
        self.guard_bulk("DELETE")?;
        let mut params = Vec::new();
        let mut sql = format!("DELETE FROM {}", self.table_clause()?);
        sql.push_str(&self.where_sql(d, &mut params, false)?);
        Ok((sql, params))
    }

    pub fn to_delete_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.delete_sql(Dialect::Sqlite)
    }

    pub fn to_insert_sql(
        &self,
        values: &[(String, SqlValue)],
    ) -> OrmResult<(String, Vec<SqlValue>)> {
        self.check()?;
        let table = quote_ident(&self.table)?;
        if values.is_empty() {
            return Ok((format!("INSERT INTO {table} DEFAULT VALUES"), Vec::new()));
        }
        let mut cols = Vec::new();
        let mut params = Vec::new();
        for (c, v) in values {
            cols.push(quote_ident(c)?);
            params.push(v.clone());
        }
        let marks = vec!["?"; cols.len()].join(", ");
        Ok((
            format!("INSERT INTO {table} ({}) VALUES ({marks})", cols.join(", ")),
            params,
        ))
    }

    /// Multi-row INSERT; every row must have the same keys. With
    /// `conflict`, appends `ON CONFLICT ...` (works on SQLite and PostgreSQL).
    fn insert_many_sql(
        &self,
        rows: &[Map<String, Value>],
        conflict: Option<(&[&str], &[&str])>,
    ) -> OrmResult<Vec<(String, Vec<SqlValue>)>> {
        self.check()?;
        let table = quote_ident(&self.table)?;
        let Some(first) = rows.first() else {
            return Ok(Vec::new());
        };
        let cols: Vec<&String> = first.keys().collect();
        if cols.is_empty() {
            return Err(OrmError::Invalid("rows to insert have no columns".into()));
        }
        let qcols = cols
            .iter()
            .map(|c| quote_ident(c))
            .collect::<OrmResult<Vec<_>>>()?;
        for r in rows {
            if r.len() != cols.len() || !cols.iter().all(|c| r.contains_key(*c)) {
                return Err(OrmError::Invalid(
                    "every row of a multi-row insert must have the same columns".into(),
                ));
            }
        }
        let tail = match conflict {
            None => String::new(),
            Some((target, update)) => {
                let t = target
                    .iter()
                    .map(|c| quote_ident(c))
                    .collect::<OrmResult<Vec<_>>>()?;
                let target_sql = if t.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", t.join(", "))
                };
                if update.is_empty() {
                    format!(" ON CONFLICT{target_sql} DO NOTHING")
                } else {
                    if t.is_empty() {
                        return Err(OrmError::Invalid("upsert needs unique_by columns".into()));
                    }
                    let sets = update
                        .iter()
                        .map(|c| quote_ident(c).map(|q| format!("{q} = excluded.{q}")))
                        .collect::<OrmResult<Vec<_>>>()?;
                    format!(" ON CONFLICT{target_sql} DO UPDATE SET {}", sets.join(", "))
                }
            }
        };
        let per_stmt = (INSERT_CHUNK_PARAMS / cols.len()).max(1);
        let row_marks = format!("({})", vec!["?"; cols.len()].join(", "));
        let mut out = Vec::new();
        for chunk in rows.chunks(per_stmt) {
            let mut params = Vec::with_capacity(chunk.len() * cols.len());
            for r in chunk {
                for c in &cols {
                    params.push(SqlValue::from_json(&r[*c]));
                }
            }
            let marks = vec![row_marks.as_str(); chunk.len()].join(", ");
            out.push((
                format!(
                    "INSERT INTO {table} ({}) VALUES {marks}{tail}",
                    qcols.join(", ")
                ),
                params,
            ));
        }
        Ok(out)
    }

    // ---------- running ----------

    pub fn get_on(&self, ex: &dyn Executor) -> OrmResult<Vec<Row>> {
        let (sql, params) = self.to_select_sql_for(ex.dialect())?;
        ex.query(&sql, &params)
    }

    pub fn first_on(&self, ex: &dyn Executor) -> OrmResult<Option<Row>> {
        Ok(self.clone().limit(1).get_on(ex)?.into_iter().next())
    }

    pub fn count_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let (sql, params) = self.to_count_sql_for(ex.dialect())?;
        let rows = ex.query(&sql, &params)?;
        Ok(rows
            .first()
            .and_then(|r| r.get("aggregate"))
            .and_then(json_as_f64)
            .unwrap_or(0.0)
            .max(0.0) as u64)
    }

    pub fn exists_on(&self, ex: &dyn Executor) -> OrmResult<bool> {
        Ok(self.first_on(ex)?.is_some())
    }

    pub fn doesnt_exist_on(&self, ex: &dyn Executor) -> OrmResult<bool> {
        Ok(!self.exists_on(ex)?)
    }

    /// `SUM`/`AVG`/`MIN`/`MAX` of a column; `Value::Null` when no rows match.
    pub fn aggregate_on(&self, ex: &dyn Executor, agg: Agg, col: &str) -> OrmResult<Value> {
        let (sql, params) = self.to_aggregate_sql(ex.dialect(), agg, col)?;
        let rows = ex.query(&sql, &params)?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|mut r| r.remove("aggregate"))
            .unwrap_or(Value::Null))
    }

    pub fn sum_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<f64> {
        Ok(json_as_f64(&self.aggregate_on(ex, Agg::Sum, col)?).unwrap_or(0.0))
    }
    pub fn avg_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Option<f64>> {
        Ok(json_as_f64(&self.aggregate_on(ex, Agg::Avg, col)?))
    }
    pub fn min_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Value> {
        self.aggregate_on(ex, Agg::Min, col)
    }
    pub fn max_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Value> {
        self.aggregate_on(ex, Agg::Max, col)
    }

    /// One column of every matching row.
    pub fn pluck_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Vec<Value>> {
        let key = result_key(col);
        let mut q = self.clone();
        q.columns = vec![col.to_string()];
        q.raw_selects.clear();
        q.sub_selects.clear();
        Ok(q.get_on(ex)?
            .into_iter()
            .map(|mut r| r.remove(&key).unwrap_or(Value::Null))
            .collect())
    }

    /// One column of the first matching row.
    pub fn value_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Option<Value>> {
        Ok(self.clone().limit(1).pluck_on(ex, col)?.into_iter().next())
    }

    /// Walk a big table in primary-key order, `size` rows at a time, without
    /// OFFSET (stable and fast even for millions of rows). Return
    /// `Ok(false)` from `f` to stop early.
    pub fn chunk_by_id_on(
        &self,
        ex: &dyn Executor,
        size: u64,
        col: &str,
        mut f: impl FnMut(Vec<Row>) -> OrmResult<bool>,
    ) -> OrmResult<()> {
        let size = size.max(1);
        let key = result_key(col);
        let mut last: Option<SqlValue> = None;
        loop {
            let mut q = self.clone().reorder().order_by(col, "asc").limit(size);
            q.offset = None;
            if let Some(l) = &last {
                q = q.where_(col, ">", l.clone());
            }
            let rows = q.get_on(ex)?;
            let n = rows.len() as u64;
            let Some(tail) = rows.last().and_then(|r| r.get(&key)) else {
                return Ok(());
            };
            last = Some(SqlValue::from_json(tail));
            if !f(rows)? || n < size {
                return Ok(());
            }
        }
    }

    pub fn update_on(&self, ex: &dyn Executor, set: Vec<(String, SqlValue)>) -> OrmResult<u64> {
        let items: Vec<Set> = set.into_iter().map(|(c, v)| Set::Value(c, v)).collect();
        let (sql, params) = self.update_sql(ex.dialect(), &items)?;
        Ok(ex.execute(&sql, &params)?.rows_affected)
    }

    /// `UPDATE t SET col = col + by [, extra...] WHERE ...` (atomic counter).
    pub fn increment_on(
        &self,
        ex: &dyn Executor,
        col: &str,
        by: impl Into<SqlValue>,
        extra: Vec<(String, SqlValue)>,
    ) -> OrmResult<u64> {
        let mut items = vec![Set::Add(col.to_string(), by.into())];
        items.extend(extra.into_iter().map(|(c, v)| Set::Value(c, v)));
        let (sql, params) = self.update_sql(ex.dialect(), &items)?;
        Ok(ex.execute(&sql, &params)?.rows_affected)
    }

    pub fn decrement_on(
        &self,
        ex: &dyn Executor,
        col: &str,
        by: i64,
        extra: Vec<(String, SqlValue)>,
    ) -> OrmResult<u64> {
        self.increment_on(ex, col, SqlValue::Int(by.saturating_neg()), extra)
    }

    pub fn delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let (sql, params) = self.delete_sql(ex.dialect())?;
        Ok(ex.execute(&sql, &params)?.rows_affected)
    }

    pub fn insert_on(
        &self,
        ex: &dyn Executor,
        values: Vec<(String, SqlValue)>,
    ) -> OrmResult<ExecResult> {
        let (sql, params) = self.to_insert_sql(&values)?;
        ex.execute(&sql, &params)
    }

    /// INSERT and return the stored row (`RETURNING *`, so defaults and
    /// generated ids are included). SQLite >= 3.35 and PostgreSQL.
    pub fn insert_returning_on(
        &self,
        ex: &dyn Executor,
        values: Vec<(String, SqlValue)>,
    ) -> OrmResult<Row> {
        let (sql, params) = self.to_insert_sql(&values)?;
        ex.query(&format!("{sql} RETURNING *"), &params)?
            .into_iter()
            .next()
            .ok_or_else(|| OrmError::Db("INSERT ... RETURNING produced no row".into()))
    }

    /// INSERT and return the new primary key (works on both databases).
    pub fn insert_get_id_on(
        &self,
        ex: &dyn Executor,
        values: Vec<(String, SqlValue)>,
        pk: &str,
    ) -> OrmResult<Value> {
        let row = self.insert_returning_on(ex, values)?;
        Ok(row.get(pk).cloned().unwrap_or(Value::Null))
    }

    /// Insert many rows (JSON objects with identical keys), chunked.
    pub fn insert_many_on(&self, ex: &dyn Executor, rows: &[Value]) -> OrmResult<u64> {
        self.run_many(ex, rows, None)
    }

    /// Insert, skipping rows that hit a unique constraint.
    pub fn insert_or_ignore_on(&self, ex: &dyn Executor, rows: &[Value]) -> OrmResult<u64> {
        self.run_many(ex, rows, Some((&[], &[])))
    }

    /// `INSERT ... ON CONFLICT (unique_by) DO UPDATE SET update...`.
    /// Empty `update` means `DO NOTHING`.
    pub fn upsert_on(
        &self,
        ex: &dyn Executor,
        rows: &[Value],
        unique_by: &[&str],
        update: &[&str],
    ) -> OrmResult<u64> {
        self.run_many(ex, rows, Some((unique_by, update)))
    }

    fn run_many(
        &self,
        ex: &dyn Executor,
        rows: &[Value],
        conflict: Option<(&[&str], &[&str])>,
    ) -> OrmResult<u64> {
        let maps = rows
            .iter()
            .map(|r| match r {
                Value::Object(m) => Ok(m.clone()),
                _ => Err(OrmError::Invalid("expected JSON objects".into())),
            })
            .collect::<OrmResult<Vec<_>>>()?;
        let mut n = 0;
        for (sql, params) in self.insert_many_sql(&maps, conflict)? {
            n += ex.execute(&sql, &params)?.rows_affected;
        }
        Ok(n)
    }

    // ---------- running on the query's connection ----------

    fn run<T>(&self, f: impl FnOnce(&dyn Executor) -> OrmResult<T>) -> OrmResult<T> {
        with_conn(self.conn.as_deref(), f)
    }

    pub fn get(&self) -> OrmResult<Vec<Row>> {
        self.run(|ex| self.get_on(ex))
    }
    pub fn first(&self) -> OrmResult<Option<Row>> {
        self.run(|ex| self.first_on(ex))
    }
    pub fn count(&self) -> OrmResult<u64> {
        self.run(|ex| self.count_on(ex))
    }
    pub fn exists(&self) -> OrmResult<bool> {
        self.run(|ex| self.exists_on(ex))
    }
    pub fn doesnt_exist(&self) -> OrmResult<bool> {
        self.run(|ex| self.doesnt_exist_on(ex))
    }
    pub fn sum(&self, col: &str) -> OrmResult<f64> {
        self.run(|ex| self.sum_on(ex, col))
    }
    pub fn avg(&self, col: &str) -> OrmResult<Option<f64>> {
        self.run(|ex| self.avg_on(ex, col))
    }
    pub fn min(&self, col: &str) -> OrmResult<Value> {
        self.run(|ex| self.min_on(ex, col))
    }
    pub fn max(&self, col: &str) -> OrmResult<Value> {
        self.run(|ex| self.max_on(ex, col))
    }
    pub fn pluck(&self, col: &str) -> OrmResult<Vec<Value>> {
        self.run(|ex| self.pluck_on(ex, col))
    }
    pub fn value(&self, col: &str) -> OrmResult<Option<Value>> {
        self.run(|ex| self.value_on(ex, col))
    }
    pub fn chunk_by_id(
        &self,
        size: u64,
        col: &str,
        f: impl FnMut(Vec<Row>) -> OrmResult<bool>,
    ) -> OrmResult<()> {
        self.run(|ex| self.chunk_by_id_on(ex, size, col, f))
    }
    pub fn update(&self, set: Vec<(String, SqlValue)>) -> OrmResult<u64> {
        self.run(|ex| self.update_on(ex, set))
    }
    pub fn increment(&self, col: &str, by: impl Into<SqlValue>) -> OrmResult<u64> {
        let by = by.into();
        self.run(|ex| self.increment_on(ex, col, by, Vec::new()))
    }
    pub fn decrement(&self, col: &str, by: i64) -> OrmResult<u64> {
        self.run(|ex| self.decrement_on(ex, col, by, Vec::new()))
    }
    pub fn delete(&self) -> OrmResult<u64> {
        self.run(|ex| self.delete_on(ex))
    }
    pub fn insert(&self, values: Vec<(String, SqlValue)>) -> OrmResult<ExecResult> {
        self.run(|ex| self.insert_on(ex, values))
    }
    pub fn insert_get_id(&self, values: Vec<(String, SqlValue)>, pk: &str) -> OrmResult<Value> {
        self.run(|ex| self.insert_get_id_on(ex, values, pk))
    }
    pub fn insert_many(&self, rows: &[Value]) -> OrmResult<u64> {
        self.run(|ex| self.insert_many_on(ex, rows))
    }
    pub fn insert_or_ignore(&self, rows: &[Value]) -> OrmResult<u64> {
        self.run(|ex| self.insert_or_ignore_on(ex, rows))
    }
    pub fn upsert(&self, rows: &[Value], unique_by: &[&str], update: &[&str]) -> OrmResult<u64> {
        self.run(|ex| self.upsert_on(ex, rows, unique_by, update))
    }
}

/// Numbers, numeric strings (PostgreSQL NUMERIC above f64 precision) -> f64.
pub(crate) fn json_as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn compile_conds(
    list: &[(Joiner, Cond)],
    d: Dialect,
    params: &mut Vec<SqlValue>,
) -> OrmResult<String> {
    let mut out = String::new();
    for (j, c) in list {
        let piece = compile_cond(c, d, params)?;
        if piece.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str(match j {
                Joiner::And => " AND ",
                Joiner::Or => " OR ",
            });
        }
        out.push_str(&piece);
    }
    Ok(out)
}

fn compile_cond(c: &Cond, d: Dialect, params: &mut Vec<SqlValue>) -> OrmResult<String> {
    Ok(match c {
        Cond::Cmp { col, op, val } => {
            params.push(val.clone());
            format!("{} {} ?", quote_ident(col)?, op_sql(op, d))
        }
        Cond::Column { a, op, b } => format!("{} {op} {}", quote_ident(a)?, quote_ident(b)?),
        Cond::In { col, vals, negate } => {
            if vals.is_empty() {
                // `x IN ()` matches nothing; `x NOT IN ()` matches everything.
                return Ok((if *negate { "1 = 1" } else { "1 = 0" }).to_string());
            }
            params.extend(vals.iter().cloned());
            format!(
                "{} {}IN ({})",
                quote_ident(col)?,
                if *negate { "NOT " } else { "" },
                vec!["?"; vals.len()].join(", ")
            )
        }
        Cond::InSub { col, sub, negate } => {
            let inner = sub.compile(d, params, &Mode::Rows, true)?;
            format!(
                "{} {}IN ({inner})",
                quote_ident(col)?,
                if *negate { "NOT " } else { "" }
            )
        }
        Cond::Exists { sub, negate } => {
            let inner = sub.compile(d, params, &Mode::One, true)?;
            format!("{}EXISTS ({inner})", if *negate { "NOT " } else { "" })
        }
        Cond::Null { col, negate } => format!(
            "{} IS {}NULL",
            quote_ident(col)?,
            if *negate { "NOT " } else { "" }
        ),
        Cond::Between {
            col,
            lo,
            hi,
            negate,
        } => {
            params.push(lo.clone());
            params.push(hi.clone());
            format!(
                "{} {}BETWEEN ? AND ?",
                quote_ident(col)?,
                if *negate { "NOT " } else { "" }
            )
        }
        Cond::Date { col, op, val } => {
            params.push(val.clone());
            let q = quote_ident(col)?;
            match d {
                Dialect::Sqlite => format!("date({q}) {op} date(?)"),
                Dialect::Postgres => format!("CAST({q} AS DATE) {op} CAST(? AS DATE)"),
            }
        }
        Cond::Json { col, path, op, val } => {
            params.push(val.clone());
            let q = quote_ident(col)?;
            // path segments were validated as identifiers / digits
            match d {
                Dialect::Sqlite => {
                    let p: String = path
                        .iter()
                        .map(|s| {
                            if s.bytes().all(|b| b.is_ascii_digit()) {
                                format!("[{s}]")
                            } else {
                                format!(".{s}")
                            }
                        })
                        .collect();
                    format!("json_extract({q}, '${p}') {} ?", op_sql(op, d))
                }
                Dialect::Postgres => {
                    let p = path.join(",");
                    format!("CAST({q} AS JSONB) #>> '{{{p}}}' {} ?", op_sql(op, d))
                }
            }
        }
        Cond::Raw { sql, params: p } => {
            params.extend(p.iter().cloned());
            format!("({sql})")
        }
        Cond::Group(inner) => {
            let s = compile_conds(inner, d, params)?;
            if s.is_empty() { s } else { format!("({s})") }
        }
    })
}

/// JSON object -> `(column, value)` pairs.
pub fn pairs_from_json(v: &Value) -> OrmResult<Vec<(String, SqlValue)>> {
    match v {
        Value::Object(m) => Ok(m
            .iter()
            .map(|(k, v)| (k.clone(), SqlValue::from_json(v)))
            .collect()),
        _ => Err(OrmError::Invalid("expected a JSON object".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sel(q: Query) -> (String, Vec<SqlValue>) {
        q.to_select_sql().unwrap()
    }
    fn pg(q: Query) -> String {
        q.to_select_sql_for(Dialect::Postgres).unwrap().0
    }

    #[test]
    fn basic_select() {
        let (sql, p) = sel(Query::new("users")
            .where_eq("id", 5)
            .order_by("name", "asc")
            .limit(10));
        assert_eq!(
            sql,
            r#"SELECT * FROM "users" WHERE "id" = ? ORDER BY "name" ASC LIMIT 10"#
        );
        assert_eq!(p, vec![SqlValue::Int(5)]);
    }

    #[test]
    fn values_never_appear_in_sql() {
        let evil = "x'; DROP TABLE users; --";
        let (sql, p) = sel(Query::new("users")
            .where_eq("name", evil)
            .where_in("id", [1, 2]));
        assert!(!sql.contains("DROP"));
        assert!(!sql.contains(evil));
        assert_eq!(p[0], SqlValue::Text(evil.into()));
    }

    #[test]
    fn identifiers_are_validated() {
        for bad in [
            "name; DROP TABLE x",
            "na me",
            "a.b.c",
            "name\"",
            "",
            "1abc",
            "a-b",
            "name--",
            "x) OR (1=1",
            "*.x",
        ] {
            let r = Query::new("users").where_eq(bad, 1).to_select_sql();
            assert!(matches!(r, Err(OrmError::Invalid(_))), "{bad}");
        }
        assert!(Query::new("bad table").to_select_sql().is_err());
        assert!(
            Query::new("users")
                .order_by("name; --", "asc")
                .to_select_sql()
                .is_err()
        );
        assert!(
            Query::new("users")
                .select(["id", "name) FROM x --"])
                .to_select_sql()
                .is_err()
        );
        assert!(
            Query::new("users")
                .select(["id as x; --"])
                .to_select_sql()
                .is_err()
        );
        assert!(
            Query::new("users")
                .join("posts; --", "a", "=", "b")
                .to_select_sql()
                .is_err()
        );
        assert!(
            Query::new("users")
                .where_json("meta->a'b", "=", 1)
                .to_select_sql()
                .is_err()
        );
        assert!(Query::new("users").alias("u u").to_select_sql().is_err());
        let (sql, _) = sel(Query::new("users").where_eq("users.id", 1));
        assert!(sql.contains(r#""users"."id" = ?"#));
    }

    #[test]
    fn operators_and_directions_are_whitelisted() {
        assert!(
            Query::new("t")
                .where_("a", "= 1 OR 1=1 --", 1)
                .to_select_sql()
                .is_err()
        );
        assert!(
            Query::new("t")
                .where_("a", "LIKE", "x%")
                .to_select_sql()
                .is_ok()
        );
        assert!(
            Query::new("t")
                .order_by("a", "asc; DROP")
                .to_select_sql()
                .is_err()
        );
        assert!(
            Query::new("t")
                .where_column("a", "like", "b")
                .to_select_sql()
                .is_err()
        );
    }

    #[test]
    fn null_comparisons_become_is_null() {
        let (sql, p) = sel(Query::new("t").where_eq("a", Option::<i32>::None).where_(
            "b",
            "!=",
            Option::<i32>::None,
        ));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "a" IS NULL AND "b" IS NOT NULL"#
        );
        assert!(p.is_empty());
    }

    #[test]
    fn empty_in_matches_nothing() {
        let (sql, _) = sel(Query::new("t").where_in("id", Vec::<i64>::new()));
        assert!(sql.ends_with("WHERE 1 = 0"));
        let (sql, _) = sel(Query::new("t").where_not_in("id", Vec::<i64>::new()));
        assert!(sql.ends_with("WHERE 1 = 1"));
    }

    #[test]
    fn groups_and_or() {
        let (sql, p) = sel(Query::new("t")
            .where_eq("a", 1)
            .where_group(|g| g.where_eq("b", 2).or_where("c", "=", 3)));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE "a" = ? AND ("b" = ? OR "c" = ?)"#
        );
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn soft_delete_filter_wraps_user_conditions() {
        let q = Query::new("t")
            .soft_deletes("deleted_at")
            .where_eq("a", 1)
            .or_where("b", "=", 2);
        let (sql, _) = sel(q.clone());
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE ("a" = ? OR "b" = ?) AND "deleted_at" IS NULL"#
        );
        let (sql, _) = sel(q.clone().with_trashed());
        assert!(!sql.contains("deleted_at"));
        let (sql, _) = sel(q.only_trashed());
        assert!(sql.ends_with(r#""deleted_at" IS NOT NULL"#));
    }

    #[test]
    fn global_scope_cannot_be_widened_by_or() {
        fn tenant(q: Query) -> Query {
            q.where_eq("tenant_id", 7)
        }
        let (sql, _) = sel(Query::new("t")
            .where_eq("a", 1)
            .or_where("b", "=", 2)
            .with_scope(tenant));
        assert_eq!(
            sql,
            r#"SELECT * FROM "t" WHERE ("a" = ? OR "b" = ?) AND ("tenant_id" = ?)"#
        );
    }

    #[test]
    fn update_and_delete_require_conditions() {
        let set = vec![("a".to_string(), SqlValue::Int(1))];
        assert!(Query::new("t").to_update_sql(&set).is_err());
        assert!(Query::new("t").to_delete_sql().is_err());
        assert!(Query::new("t").all_rows().to_delete_sql().is_ok());
        let (sql, p) = Query::new("t")
            .where_eq("id", 9)
            .to_update_sql(&set)
            .unwrap();
        assert_eq!(sql, r#"UPDATE "t" SET "a" = ? WHERE "id" = ?"#);
        assert_eq!(p, vec![SqlValue::Int(1), SqlValue::Int(9)]);
        // a soft-delete filter alone does not count as a condition
        let q = Query::new("t").soft_deletes("deleted_at");
        assert!(q.to_delete_sql().is_err());
        // joins are refused for bulk writes
        let j = Query::new("t")
            .join("u", "u.id", "=", "t.u")
            .where_eq("t.id", 1);
        assert!(j.to_delete_sql().is_err());
    }

    #[test]
    fn insert_sql() {
        let (sql, p) = Query::new("t")
            .to_insert_sql(&[
                ("a".into(), SqlValue::Int(1)),
                ("b".into(), SqlValue::Text("x".into())),
            ])
            .unwrap();
        assert_eq!(sql, r#"INSERT INTO "t" ("a", "b") VALUES (?, ?)"#);
        assert_eq!(p.len(), 2);
        let (sql, _) = Query::new("t").to_insert_sql(&[]).unwrap();
        assert_eq!(sql, r#"INSERT INTO "t" DEFAULT VALUES"#);
    }

    #[test]
    fn multi_row_insert_and_upsert_sql() {
        let rows: Vec<Map<String, Value>> = [json!({"a": 1, "b": "x"}), json!({"a": 2, "b": "y"})]
            .into_iter()
            .map(|v| v.as_object().unwrap().clone())
            .collect();
        let q = Query::new("t");
        let s = q.insert_many_sql(&rows, None).unwrap();
        assert_eq!(
            s[0].0,
            r#"INSERT INTO "t" ("a", "b") VALUES (?, ?), (?, ?)"#
        );
        assert_eq!(s[0].1.len(), 4);
        let s = q.insert_many_sql(&rows, Some((&["a"], &["b"]))).unwrap();
        assert!(
            s[0].0
                .ends_with(r#"ON CONFLICT ("a") DO UPDATE SET "b" = excluded."b""#)
        );
        let s = q.insert_many_sql(&rows, Some((&[], &[]))).unwrap();
        assert!(s[0].0.ends_with("ON CONFLICT DO NOTHING"));
        let ragged = vec![
            rows[0].clone(),
            json!({"a": 3}).as_object().unwrap().clone(),
        ];
        assert!(q.insert_many_sql(&ragged, None).is_err());
    }

    #[test]
    fn joins_groups_aggregates() {
        let q = Query::new("posts")
            .select(["users.name as author", "posts.*"])
            .select_raw("COUNT(*) AS n", vec![])
            .join("users", "users.id", "=", "posts.user_id")
            .where_eq("posts.published", true)
            .group_by(["users.name"])
            .having_raw("COUNT(*) > ?", vec![SqlValue::Int(1)]);
        let (sql, p) = sel(q);
        assert_eq!(
            sql,
            r#"SELECT "users"."name" AS "author", "posts".*, COUNT(*) AS n FROM "posts" INNER JOIN "users" ON "users"."id" = "posts"."user_id" WHERE "posts"."published" = ? GROUP BY "users"."name" HAVING (COUNT(*) > ?)"#
        );
        assert_eq!(p, vec![SqlValue::Bool(true), SqlValue::Int(1)]);

        let (c, _) = Query::new("t").group_by(["a"]).to_count_sql().unwrap();
        assert!(c.starts_with(
            r#"SELECT COUNT(*) AS "aggregate" FROM (SELECT "a" FROM "t" GROUP BY "a")"#
        ));
        let (a, _) = Query::new("t")
            .where_eq("x", 1)
            .order_by("id", "asc")
            .to_aggregate_sql(Dialect::Sqlite, Agg::Sum, "amount")
            .unwrap();
        assert_eq!(
            a,
            r#"SELECT SUM("amount") AS "aggregate" FROM "t" WHERE "x" = ?"#
        );
    }

    #[test]
    fn dialect_specific_bits() {
        let q = Query::new("t").where_like("name", "%ada%");
        assert!(sel(q.clone()).0.contains(r#""name" LIKE ?"#));
        assert!(pg(q).contains(r#""name" ILIKE ?"#));

        let q = Query::new("t").where_json("meta->theme", "=", "dark");
        assert!(
            sel(q.clone())
                .0
                .contains(r#"json_extract("meta", '$.theme') = ?"#)
        );
        assert!(pg(q).contains(r#"CAST("meta" AS JSONB) #>> '{theme}' = ?"#));

        let q = Query::new("t").where_json("meta->tags->0", "=", "x");
        assert!(sel(q).0.contains("'$.tags[0]'"));

        let q = Query::new("t").where_eq("id", 1).lock_for_update();
        assert!(!sel(q.clone()).0.contains("FOR UPDATE"));
        assert!(pg(q).ends_with("FOR UPDATE"));

        let q = Query::new("t").where_date("created_at", ">=", "2026-01-01");
        assert!(
            sel(q.clone())
                .0
                .contains(r#"date("created_at") >= date(?)"#)
        );
        assert!(pg(q).contains(r#"CAST("created_at" AS DATE) >= CAST(? AS DATE)"#));
    }

    #[test]
    fn subqueries_keep_parameter_order() {
        let sub = Query::new("comments")
            .where_column("comments.post_id", "=", "posts.id")
            .where_eq("approved", true);
        let q = Query::new("posts")
            .where_eq("a", 1)
            .select_sub(
                sub.clone(),
                SubSelect::Agg(Agg::Count, None),
                "comments_count".into(),
            )
            .where_exists(sub)
            .where_in_sub(
                "user_id",
                Query::new("users").select(["id"]).where_eq("admin", false),
            );
        let (sql, p) = sel(q);
        assert_eq!(
            sql,
            r#"SELECT "posts".*, (SELECT COUNT(*) FROM "comments" WHERE "comments"."post_id" = "posts"."id" AND "approved" = ?) AS "comments_count" FROM "posts" WHERE "a" = ? AND EXISTS (SELECT 1 FROM "comments" WHERE "comments"."post_id" = "posts"."id" AND "approved" = ?) AND "user_id" IN (SELECT "id" FROM "users" WHERE "admin" = ?)"#
        );
        assert_eq!(
            p,
            vec![
                SqlValue::Bool(true),
                SqlValue::Int(1),
                SqlValue::Bool(true),
                SqlValue::Bool(false)
            ]
        );
    }

    #[test]
    fn soft_deletes_are_qualified_in_subqueries_and_joins() {
        let sub = Query::new("posts").soft_deletes("deleted_at").where_column(
            "posts.user_id",
            "=",
            "users.id",
        );
        let (sql, _) = sel(Query::new("users").where_exists(sub));
        assert!(sql.contains(
            r#"WHERE ("posts"."user_id" = "users"."id") AND "posts"."deleted_at" IS NULL"#
        ));
        let (sql, _) = sel(Query::new("posts").soft_deletes("deleted_at").join(
            "users",
            "users.id",
            "=",
            "posts.user_id",
        ));
        assert!(sql.ends_with(r#"WHERE "posts"."deleted_at" IS NULL"#));
    }

    #[test]
    fn sort_by_whitelist() {
        let ok = Query::new("t").sort_by(Some("-title"), &["title", "id"], ("id", "asc"));
        assert!(sel(ok).0.ends_with(r#"ORDER BY "title" DESC"#));
        let dflt = Query::new("t").sort_by(None, &["title"], ("id", "desc"));
        assert!(sel(dflt).0.ends_with(r#"ORDER BY "id" DESC"#));
        let bad = Query::new("t").sort_by(Some("password"), &["title"], ("id", "asc"));
        assert!(bad.to_select_sql().is_err());
    }

    #[test]
    fn offset_without_limit_and_huge_values() {
        let (sql, _) = sel(Query::new("t").offset(5));
        assert!(sql.ends_with(&format!("LIMIT {} OFFSET 5", i64::MAX)));
        let (sql, _) = sel(Query::new("t").limit(u64::MAX).offset(u64::MAX));
        assert!(sql.contains(&i64::MAX.to_string()));
    }

    #[test]
    fn raw_conditions_bind_params() {
        let (sql, p) = sel(Query::new("t").where_raw("a + b > ?", vec![SqlValue::Int(3)]));
        assert_eq!(sql, r#"SELECT * FROM "t" WHERE (a + b > ?)"#);
        assert_eq!(p, vec![SqlValue::Int(3)]);
    }

    #[test]
    fn when_and_increment() {
        let q = Query::new("t")
            .when(false, |q| q.where_eq("a", 1))
            .when(true, |q| q.where_eq("b", 2));
        assert_eq!(sel(q.clone()).0, r#"SELECT * FROM "t" WHERE "b" = ?"#);
        let (sql, p) = q
            .update_sql(
                Dialect::Sqlite,
                &[Set::Add("views".into(), SqlValue::Int(1))],
            )
            .unwrap();
        assert_eq!(sql, r#"UPDATE "t" SET "views" = "views" + ? WHERE "b" = ?"#);
        assert_eq!(p.len(), 2);
    }
}
