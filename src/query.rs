//! The query builder.
//!
//! Security model:
//!   * values are always bound parameters (`?`), never spliced into SQL;
//!   * table and column names are validated (`[A-Za-z_][A-Za-z0-9_]*`, at
//!     most `table.column`) and double-quoted;
//!   * operators and sort directions come from a fixed whitelist;
//!   * the only way to inject SQL text is `where_raw`, which takes a
//!     `&'static str`, so request data cannot reach it;
//!   * `update` / `delete` without a `WHERE` are refused unless you call
//!     `all_rows()`;
//!   * an empty `IN ()` matches nothing (never everything).
//!
//! Builder methods never fail while chaining: a bad identifier is
//! remembered and reported by the terminal call (`get`, `first`, ...).

use crate::conn::{ExecResult, Executor, with_db};
use crate::error::{OrmError, OrmResult};
use crate::value::{Row, SqlValue};
use serde_json::Value;

const MAX_IN_LIST: usize = 10_000;

/// Validate and double-quote `name` or `table.column`.
pub(crate) fn quote_ident(s: &str) -> OrmResult<String> {
    if s == "*" {
        return Ok("*".to_string());
    }
    let part_ok = |p: &str| {
        !p.is_empty()
            && p.len() <= 64
            && p.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() > 2 || !parts.iter().all(|p| part_ok(p)) {
        let shown: String = s.chars().take(40).collect();
        return Err(OrmError::Invalid(format!("invalid identifier `{shown}`")));
    }
    Ok(parts
        .iter()
        .map(|p| format!("\"{p}\""))
        .collect::<Vec<_>>()
        .join("."))
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
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joiner {
    And,
    Or,
}

#[derive(Clone, Debug)]
enum Cond {
    Cmp {
        col: String,
        op: &'static str,
        val: SqlValue,
    },
    In {
        col: String,
        vals: Vec<SqlValue>,
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

#[derive(Clone, Debug)]
pub struct Query {
    table: String,
    columns: Vec<String>,
    wheres: Vec<(Joiner, Cond)>,
    orders: Vec<(String, bool)>,
    limit: Option<u64>,
    offset: Option<u64>,
    soft_col: Option<&'static str>,
    trashed: Trashed,
    all_rows: bool,
    error: Option<OrmError>,
}

impl Query {
    pub fn new(table: &str) -> Self {
        Query {
            table: table.to_string(),
            columns: Vec::new(),
            wheres: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
            soft_col: None,
            trashed: Trashed::Without,
            all_rows: false,
            error: None,
        }
    }

    pub(crate) fn soft_deletes(mut self, column: &'static str) -> Self {
        self.soft_col = Some(column);
        self
    }

    fn fail(&mut self, e: OrmError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
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

    // ---------- selecting ----------

    /// Restrict the selected columns (default `*`).
    pub fn select<I, S>(mut self, cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for c in cols {
            if self.ident_ok(c.as_ref()) {
                self.columns.push(c.as_ref().to_string());
            }
        }
        self
    }

    // ---------- where ----------

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
        self.wheres.push((j, cond));
        self
    }

    /// `WHERE col <op> ?`. Operators: `= != <> < <= > >= like "not like"`.
    /// Comparing with `NULL` becomes `IS NULL` / `IS NOT NULL`.
    pub fn where_(self, col: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::And, col, op, val.into())
    }

    pub fn where_eq(self, col: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::And, col, "=", val.into())
    }

    pub fn or_where(self, col: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        self.add_cmp(Joiner::Or, col, op, val.into())
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
        self.wheres.push((
            j,
            Cond::In {
                col: col.to_string(),
                vals,
                negate,
            },
        ));
        self
    }

    pub fn where_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.add_in(Joiner::And, col, vals, false)
    }

    pub fn where_not_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.add_in(Joiner::And, col, vals, true)
    }

    pub fn where_null(mut self, col: &str) -> Self {
        if self.ident_ok(col) {
            self.wheres.push((
                Joiner::And,
                Cond::Null {
                    col: col.to_string(),
                    negate: false,
                },
            ));
        }
        self
    }

    pub fn where_not_null(mut self, col: &str) -> Self {
        if self.ident_ok(col) {
            self.wheres.push((
                Joiner::And,
                Cond::Null {
                    col: col.to_string(),
                    negate: true,
                },
            ));
        }
        self
    }

    pub fn where_between(
        mut self,
        col: &str,
        lo: impl Into<SqlValue>,
        hi: impl Into<SqlValue>,
    ) -> Self {
        if self.ident_ok(col) {
            self.wheres.push((
                Joiner::And,
                Cond::Between {
                    col: col.to_string(),
                    lo: lo.into(),
                    hi: hi.into(),
                },
            ));
        }
        self
    }

    /// A raw SQL condition with `?` placeholders. The SQL must be a string
    /// literal in your source (`&'static str`), which makes it impossible
    /// to pass request data as SQL text by accident.
    pub fn where_raw(mut self, sql: &'static str, params: Vec<SqlValue>) -> Self {
        self.wheres.push((Joiner::And, Cond::Raw { sql, params }));
        self
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
            self.orders.push((col.to_string(), desc));
        }
        self
    }

    pub fn order_by_desc(self, col: &str) -> Self {
        self.order_by(col, "desc")
    }

    /// Newest first by `created_at`.
    pub fn latest(self) -> Self {
        self.order_by("created_at", "desc")
    }

    /// Sort by a client-supplied value, safely. `requested` is e.g. the
    /// `?sort=` parameter: `"name"` (ascending) or `"-name"` (descending).
    /// Anything not in `allowed` makes the query fail with a 400.
    ///
    /// ```ignore
    /// Post::query().sort_by(req.query_param("sort").as_deref(), &["title", "created_at"], ("id", "asc"))
    /// ```
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

    pub fn offset(mut self, n: u64) -> Self {
        self.offset = Some(n);
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

    fn where_sql(&self, params: &mut Vec<SqlValue>) -> OrmResult<String> {
        let user = compile_conds(&self.wheres, params)?;
        let soft = match (self.soft_col, self.trashed) {
            (Some(c), Trashed::Without) => Some(format!("{} IS NULL", quote_ident(c)?)),
            (Some(c), Trashed::Only) => Some(format!("{} IS NOT NULL", quote_ident(c)?)),
            _ => None,
        };
        Ok(match (user.is_empty(), soft) {
            (true, None) => String::new(),
            (true, Some(s)) => format!(" WHERE {s}"),
            (false, None) => format!(" WHERE {user}"),
            (false, Some(s)) => format!(" WHERE ({user}) AND {s}"),
        })
    }

    pub fn to_select_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.check()?;
        let mut params = Vec::new();
        let cols = if self.columns.is_empty() {
            "*".to_string()
        } else {
            self.columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<OrmResult<Vec<_>>>()?
                .join(", ")
        };
        let mut sql = format!("SELECT {cols} FROM {}", quote_ident(&self.table)?);
        sql.push_str(&self.where_sql(&mut params)?);

        if !self.orders.is_empty() {
            let parts = self
                .orders
                .iter()
                .map(|(c, desc)| {
                    quote_ident(c).map(|q| format!("{q} {}", if *desc { "DESC" } else { "ASC" }))
                })
                .collect::<OrmResult<Vec<_>>>()?;
            sql.push_str(&format!(" ORDER BY {}", parts.join(", ")));
        }
        match (self.limit, self.offset) {
            (Some(l), Some(o)) => sql.push_str(&format!(
                " LIMIT {} OFFSET {}",
                l.min(i64::MAX as u64),
                o.min(i64::MAX as u64)
            )),
            (Some(l), None) => sql.push_str(&format!(" LIMIT {}", l.min(i64::MAX as u64))),
            (None, Some(o)) => sql.push_str(&format!(
                " LIMIT {} OFFSET {}",
                i64::MAX,
                o.min(i64::MAX as u64)
            )),
            (None, None) => {}
        }
        Ok((sql, params))
    }

    pub fn to_count_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.check()?;
        let mut params = Vec::new();
        let mut sql = format!(
            "SELECT COUNT(*) AS \"aggregate\" FROM {}",
            quote_ident(&self.table)?
        );
        sql.push_str(&self.where_sql(&mut params)?);
        Ok((sql, params))
    }

    pub fn to_update_sql(&self, set: &[(String, SqlValue)]) -> OrmResult<(String, Vec<SqlValue>)> {
        self.check()?;
        if self.wheres.is_empty() && !self.all_rows {
            return Err(OrmError::Invalid(
                "refusing to UPDATE every row; add a condition or call all_rows()".into(),
            ));
        }
        if set.is_empty() {
            return Err(OrmError::Invalid("nothing to update".into()));
        }
        let mut params = Vec::new();
        let mut sets = Vec::new();
        for (c, v) in set {
            sets.push(format!("{} = ?", quote_ident(c)?));
            params.push(v.clone());
        }
        let mut sql = format!(
            "UPDATE {} SET {}",
            quote_ident(&self.table)?,
            sets.join(", ")
        );
        sql.push_str(&self.where_sql(&mut params)?);
        Ok((sql, params))
    }

    pub fn to_delete_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.check()?;
        if self.wheres.is_empty() && !self.all_rows {
            return Err(OrmError::Invalid(
                "refusing to DELETE every row; add a condition or call all_rows()".into(),
            ));
        }
        let mut params = Vec::new();
        let mut sql = format!("DELETE FROM {}", quote_ident(&self.table)?);
        sql.push_str(&self.where_sql(&mut params)?);
        Ok((sql, params))
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

    // ---------- running ----------

    pub fn get_on(&self, ex: &dyn Executor) -> OrmResult<Vec<Row>> {
        let (sql, params) = self.to_select_sql()?;
        ex.query(&sql, &params)
    }

    pub fn first_on(&self, ex: &dyn Executor) -> OrmResult<Option<Row>> {
        Ok(self.clone().limit(1).get_on(ex)?.into_iter().next())
    }

    pub fn count_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let (sql, params) = self.to_count_sql()?;
        let rows = ex.query(&sql, &params)?;
        Ok(rows
            .first()
            .and_then(|r| r.get("aggregate"))
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0) as u64)
    }

    pub fn exists_on(&self, ex: &dyn Executor) -> OrmResult<bool> {
        Ok(self.count_on(ex)? > 0)
    }

    pub fn update_on(&self, ex: &dyn Executor, set: Vec<(String, SqlValue)>) -> OrmResult<u64> {
        let (sql, params) = self.to_update_sql(&set)?;
        Ok(ex.execute(&sql, &params)?.rows_affected)
    }

    pub fn delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let (sql, params) = self.to_delete_sql()?;
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

    pub fn get(&self) -> OrmResult<Vec<Row>> {
        with_db(|ex| self.get_on(ex))
    }
    pub fn first(&self) -> OrmResult<Option<Row>> {
        with_db(|ex| self.first_on(ex))
    }
    pub fn count(&self) -> OrmResult<u64> {
        with_db(|ex| self.count_on(ex))
    }
    pub fn exists(&self) -> OrmResult<bool> {
        with_db(|ex| self.exists_on(ex))
    }
    pub fn update(&self, set: Vec<(String, SqlValue)>) -> OrmResult<u64> {
        with_db(|ex| self.update_on(ex, set))
    }
    pub fn delete(&self) -> OrmResult<u64> {
        with_db(|ex| self.delete_on(ex))
    }
    pub fn insert(&self, values: Vec<(String, SqlValue)>) -> OrmResult<ExecResult> {
        with_db(|ex| self.insert_on(ex, values))
    }
}

fn compile_conds(list: &[(Joiner, Cond)], params: &mut Vec<SqlValue>) -> OrmResult<String> {
    let mut out = String::new();
    for (j, c) in list {
        let piece = compile_cond(c, params)?;
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

fn compile_cond(c: &Cond, params: &mut Vec<SqlValue>) -> OrmResult<String> {
    Ok(match c {
        Cond::Cmp { col, op, val } => {
            params.push(val.clone());
            format!("{} {op} ?", quote_ident(col)?)
        }
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
        Cond::Null { col, negate } => format!(
            "{} IS {}NULL",
            quote_ident(col)?,
            if *negate { "NOT " } else { "" }
        ),
        Cond::Between { col, lo, hi } => {
            params.push(lo.clone());
            params.push(hi.clone());
            format!("{} BETWEEN ? AND ?", quote_ident(col)?)
        }
        Cond::Raw { sql, params: p } => {
            params.extend(p.iter().cloned());
            format!("({sql})")
        }
        Cond::Group(inner) => {
            let s = compile_conds(inner, params)?;
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

    fn sel(q: Query) -> (String, Vec<SqlValue>) {
        q.to_select_sql().unwrap()
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
}
