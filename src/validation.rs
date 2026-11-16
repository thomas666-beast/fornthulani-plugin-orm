//! `unique:` and `exists:` validation rules (feature `validation`).
//!
//! The rules go through the ORM's query builder, so they work on every
//! driver (SQLite and PostgreSQL) and on every named connection.
//!
//! ```ignore
//! fornthulani_plugin_orm::register_validation_rules();   // once, at startup
//!
//! req.validate([
//!     ("email",   "required|email|unique:users,email"),
//!     ("team_id", "required|integer|exists:teams,id"),
//! ])?;
//! ```
//!
//! Syntax (Laravel-compatible):
//!
//! ```text
//! unique:[connection.]table[,column[,ignore[,id_column[,where_col,where_val]...]]]
//! exists:[connection.]table[,column[,where_col,where_val]...]
//! ```
//!
//! * `column` defaults to the attribute name (last segment for `a.b.c`).
//! * `ignore` skips the row being edited (`unique:users,email,{id}`);
//!   empty or `NULL` means "ignore nothing".
//! * Extra `where` pairs narrow the check: a value of `NULL` means
//!   `IS NULL`, `NOT_NULL` means `IS NOT NULL`, `!x` means `!= x`;
//!   `true` / `false` and integers are typed.
//!   `unique:users,email,NULL,id,deleted_at,NULL` ignores soft-deleted rows.
//! * `connection.table` uses a connection registered with
//!   `Db::add_connection`; if no such connection exists the prefix is a
//!   PostgreSQL schema (`exists:billing.plans,code`).
//! * `exists` on an array checks that every distinct element exists.
//!
//! Both rules fail closed: a missing table, bad identifier or database
//! error reports the value as invalid. Identifiers go through the
//! builder's quoting and validation, values are bound parameters.

use crate::conn::Db;
use crate::query::Query;
use crate::value::SqlValue;
use fornthulani_plugin_validate::{ExtCtx, extend};
use serde_json::Value;
use std::collections::HashSet;

pub fn register_validation_rules() {
    extend(
        "unique",
        unique_rule,
        "The :attribute has already been taken.",
    );
    extend("exists", exists_rule, "The selected :attribute is invalid.");
}

/// Resolve `[conn.]table` into a query on the right connection.
fn base_query(spec: &str) -> Query {
    if let Some((conn, table)) = spec.split_once('.')
        && Db::has_connection(conn)
    {
        return Query::new(table).on(conn);
    }
    Query::new(spec)
}

fn column<'a>(ctx: &'a ExtCtx<'_>, idx: usize) -> &'a str {
    match ctx
        .args
        .get(idx)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        Some(c) => c,
        None => ctx.attribute.rsplit('.').next().unwrap_or(ctx.attribute),
    }
}

fn typed(v: &str) -> SqlValue {
    match v {
        "true" => SqlValue::Bool(true),
        "false" => SqlValue::Bool(false),
        _ => match v.parse::<i64>() {
            Ok(n) => SqlValue::Int(n),
            Err(_) => SqlValue::Text(v.to_string()),
        },
    }
}

/// Apply `col,val` pairs; `None` when the pairs are malformed.
fn apply_wheres(mut q: Query, pairs: &[String]) -> Option<Query> {
    if !pairs.len().is_multiple_of(2) {
        return None;
    }
    for pair in pairs.chunks(2) {
        let (col, val) = (pair[0].trim(), pair[1].trim());
        if col.is_empty() {
            return None;
        }
        q = match val {
            "NULL" => q.where_null(col),
            "NOT_NULL" => q.where_not_null(col),
            v if v.starts_with('!') => q.where_(col, "!=", typed(&v[1..])),
            v => q.where_eq(col, typed(v)),
        };
    }
    Some(q)
}

fn scalar(v: &Value) -> Option<SqlValue> {
    match v {
        Value::Null | Value::Array(_) | Value::Object(_) => None,
        other => Some(SqlValue::from_json(other)),
    }
}

fn unique_rule(ctx: &ExtCtx<'_>) -> bool {
    let Some(table) = ctx.args.first() else {
        return false;
    };
    let Some(value) = scalar(ctx.value) else {
        return false;
    };
    let col = column(ctx, 1);
    let mut q = base_query(table.trim()).where_eq(col, value);
    if let Some(ignore) = ctx.args.get(2).map(|s| s.trim())
        && !ignore.is_empty()
        && ignore != "NULL"
    {
        let id_col = ctx
            .args
            .get(3)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or("id");
        q = q.where_(id_col, "!=", typed(ignore));
    }
    let extra = ctx.args.get(4..).unwrap_or(&[]);
    let Some(q) = apply_wheres(q, extra) else {
        return false;
    };
    matches!(q.exists(), Ok(false))
}

fn key(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn exists_rule(ctx: &ExtCtx<'_>) -> bool {
    let Some(table) = ctx.args.first() else {
        return false;
    };
    let col = column(ctx, 1);
    let extra = ctx.args.get(2..).unwrap_or(&[]);
    let Some(q) = apply_wheres(base_query(table.trim()), extra) else {
        return false;
    };
    match ctx.value {
        Value::Array(items) => {
            let mut wanted = HashSet::new();
            let mut values = Vec::new();
            for item in items {
                let (Some(k), Some(v)) = (key(item), scalar(item)) else {
                    return false;
                };
                if wanted.insert(k) {
                    values.push(v);
                }
            }
            if values.is_empty() {
                return true;
            }
            let Ok(found) = q.where_in(col, values).distinct().pluck(col) else {
                return false;
            };
            let found: HashSet<String> = found.iter().filter_map(key).collect();
            wanted.iter().all(|k| found.contains(k))
        }
        v => match scalar(v) {
            Some(val) => matches!(q.where_eq(col, val).exists(), Ok(true)),
            None => false,
        },
    }
}
