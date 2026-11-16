//! `unique:` and `exists:` validation rules (feature `validation`).
//!
//! ```ignore
//! fornthulani_plugin_orm::register_validation_rules();   // once, at startup
//!
//! req.validate([
//!     ("email", "required|email|unique:users,email"),
//!     ("team_id", "required|integer|exists:teams,id"),
//! ])?;
//!
//! // On update, ignore the row being edited: unique:table,column,ignore_id[,id_column]
//! ("email", format!("required|email|unique:users,email,{id}"))
//! ```
//!
//! Both fail closed: if the database cannot be queried the value is
//! reported invalid.

use crate::conn::Db;
use crate::value::SqlValue;
use fornthulani_plugin_validate::{ExtCtx, extend};

pub fn register_validation_rules() {
    extend(
        "unique",
        unique_rule,
        "The :attribute has already been taken.",
    );
    extend("exists", exists_rule, "The selected :attribute is invalid.");
}

fn unique_rule(ctx: &ExtCtx<'_>) -> bool {
    let (Some(table), Some(column)) = (ctx.args.first(), ctx.args.get(1)) else {
        return false;
    };
    let mut q = Db::table(table).where_eq(column, SqlValue::from_json(ctx.value));
    if let Some(ignore) = ctx.args.get(2) {
        let id_col = ctx.args.get(3).map(String::as_str).unwrap_or("id");
        let ignore_val = match ignore.parse::<i64>() {
            Ok(n) => SqlValue::Int(n),
            Err(_) => SqlValue::Text(ignore.clone()),
        };
        q = q.where_(id_col, "!=", ignore_val);
    }
    matches!(q.count(), Ok(0))
}

fn exists_rule(ctx: &ExtCtx<'_>) -> bool {
    let (Some(table), Some(column)) = (ctx.args.first(), ctx.args.get(1)) else {
        return false;
    };
    let q = Db::table(table).where_eq(column, SqlValue::from_json(ctx.value));
    matches!(q.count(), Ok(n) if n > 0)
}
