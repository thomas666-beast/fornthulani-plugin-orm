//! Eloquent-style models.
//!
//! ```ignore
//! #[derive(Serialize, Deserialize)]
//! struct Post {
//!     id: Option<i64>,
//!     user_id: i64,
//!     title: String,
//!     published: bool,
//!     meta: Option<serde_json::Value>,
//!     created_at: Option<String>,
//!     updated_at: Option<String>,
//!     #[serde(default, skip_serializing_if = "Option::is_none")]
//!     comments: Option<Vec<Comment>>,
//! }
//!
//! impl Model for Post {
//!     const TABLE: &'static str = "posts";
//!     const FILLABLE: &'static [&'static str] = &["title", "published", "meta"];
//!     const CASTS: &'static [(&'static str, Cast)] = &[("published", Cast::Bool), ("meta", Cast::Json)];
//!     fn id(&self) -> Option<i64> { self.id }
//!     fn relations() -> Vec<Relation> {
//!         vec![Relation::has_many::<Comment>("comments", "post_id")]
//!     }
//! }
//!
//! let posts = Post::query().with(&["comments"]).where_has("comments", |q| q.where_eq("approved", true)).get()?;
//! ```

use crate::conn::{Executor, transaction_in, with_conn};
use crate::error::{OrmError, OrmResult};
use crate::query::{Agg, Joiner, Query, json_as_f64, pairs_from_json};
use crate::relation::{
    Constraint, Kind, Relation, SyncResult, With, add_aggregate, add_exists_select, add_has,
    eager_load, find_relation, non_column_keys, strip_hidden,
};
use crate::time::now_iso;
use crate::value::{Row, SqlValue};
use fornthulani::{Response, Status};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::marker::PhantomData;
use std::sync::Arc;

/// Hard cap for `paginate`, so `?per_page=1000000` cannot dump a table.
pub const MAX_PER_PAGE: u64 = 100;

/// Attribute casts, applied to every row read (Laravel's `$casts`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cast {
    /// `0/1`, `"t"/"f"`, `"true"/"false"` -> bool (SQLite stores 0/1).
    Bool,
    /// Numeric strings / floats -> integer.
    Int,
    /// Numeric strings (e.g. PostgreSQL NUMERIC) / integers -> float.
    Float,
    /// Numbers -> string (keep `NUMERIC` money exact: cast to String).
    String,
    /// JSON text -> JSON value (SQLite stores JSON as text; PostgreSQL JSONB
    /// already arrives as JSON and is left alone).
    Json,
}

fn apply_cast(v: &mut Value, cast: Cast) {
    let new = match (cast, &*v) {
        (_, Value::Null) => return,
        (Cast::Bool, Value::Number(n)) => Value::Bool(n.as_f64().is_some_and(|f| f != 0.0)),
        (Cast::Bool, Value::String(s)) => match s.to_ascii_lowercase().as_str() {
            "1" | "t" | "true" | "yes" | "on" => Value::Bool(true),
            "0" | "f" | "false" | "no" | "off" | "" => Value::Bool(false),
            _ => return,
        },
        (Cast::Int, Value::String(s)) => match s.trim().parse::<i64>() {
            Ok(i) => Value::from(i),
            Err(_) => return,
        },
        (Cast::Int, Value::Number(n)) if n.as_i64().is_none() => match n.as_f64() {
            Some(f) if f.is_finite() => Value::from(f.trunc() as i64),
            _ => return,
        },
        (Cast::Int, Value::Bool(b)) => Value::from(i64::from(*b)),
        (Cast::Float, Value::String(s)) => match s.trim().parse::<f64>() {
            Ok(f) if f.is_finite() => Value::from(f),
            _ => return,
        },
        (Cast::Float, Value::Number(n)) => match n.as_f64() {
            Some(f) => Value::from(f),
            None => return,
        },
        (Cast::String, Value::Number(n)) => Value::String(n.to_string()),
        (Cast::String, Value::Bool(b)) => Value::String(b.to_string()),
        (Cast::Json, Value::String(s)) => match serde_json::from_str(s) {
            Ok(j) => j,
            Err(_) => return,
        },
        _ => return,
    };
    *v = new;
}

fn to_object<T: Serialize>(attrs: &T) -> OrmResult<Map<String, Value>> {
    match serde_json::to_value(attrs) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(OrmError::Invalid("attributes must be an object".into())),
        Err(e) => Err(OrmError::Invalid(format!(
            "attributes are not serialisable: {e}"
        ))),
    }
}

fn not_found<M: Model>() -> OrmError {
    OrmError::NotFound(format!("{} not found", M::TABLE))
}

pub trait Model: Serialize + DeserializeOwned + Sized {
    const TABLE: &'static str;
    const PRIMARY_KEY: &'static str = "id";
    /// Maintain `created_at` / `updated_at` (ISO-8601 UTC).
    const TIMESTAMPS: bool = true;
    /// Use a `deleted_at` column instead of really deleting.
    const SOFT_DELETES: bool = false;
    /// Mass-assignment allow-list for `create` / `update` / `upsert`.
    /// **Empty by default: nothing is fillable until you say so**, so a
    /// client can never set `is_admin` just by sending it.
    const FILLABLE: &'static [&'static str] = &[];
    /// Columns removed by `public_json` (passwords, tokens, ...), also
    /// inside eager-loaded relations.
    const HIDDEN: &'static [&'static str] = &[];
    /// `bool` columns (shorthand for `Cast::Bool`).
    const BOOLS: &'static [&'static str] = &[];
    /// Attribute casts applied on read.
    const CASTS: &'static [(&'static str, Cast)] = &[];
    /// Named connection (`Db::add_connection`); `None` = default.
    const CONNECTION: Option<&'static str> = None;
    /// Default page size for `paginate_default`.
    const PER_PAGE: u64 = 15;

    /// The primary key of this instance (`None` before it is saved).
    fn id(&self) -> Option<i64>;

    /// This model's relationships (see [`Relation`]).
    fn relations() -> Vec<Relation> {
        Vec::new()
    }

    /// A scope applied to every query of this model (multi-tenancy,
    /// `published = true`, ...). Skip it with `without_global_scope()`.
    /// Its conditions are parenthesised separately from yours, so an
    /// `or_where` can never escape it.
    fn global_scope(q: Query) -> Query {
        q
    }

    // ---------- lifecycle hooks (Laravel model events) ----------
    // Return an error to abort the write. They receive the executor so they
    // can write audit rows inside the same transaction.

    /// Before INSERT; may change the attributes (slug, uuid, defaults).
    fn creating(_attrs: &mut Map<String, Value>, _ex: &dyn Executor) -> OrmResult<()> {
        Ok(())
    }
    fn created(&self, _ex: &dyn Executor) -> OrmResult<()> {
        Ok(())
    }
    /// Before UPDATE; `changes` are the columns about to be written.
    fn updating(&self, _changes: &mut Map<String, Value>, _ex: &dyn Executor) -> OrmResult<()> {
        Ok(())
    }
    fn updated(&self, _ex: &dyn Executor) -> OrmResult<()> {
        Ok(())
    }
    fn deleting(&self, _ex: &dyn Executor) -> OrmResult<()> {
        Ok(())
    }
    fn deleted(&self, _ex: &dyn Executor) -> OrmResult<()> {
        Ok(())
    }

    // ------------------------------------------------------------
    // plumbing
    // ------------------------------------------------------------

    /// Run `f` on this model's connection.
    #[doc(hidden)]
    fn run<T>(f: impl FnOnce(&dyn Executor) -> OrmResult<T>) -> OrmResult<T> {
        with_conn(Self::CONNECTION, f)
    }

    /// Apply `BOOLS` and `CASTS` to a raw row.
    fn cast_row(row: &mut Row) {
        for col in Self::BOOLS {
            if let Some(v) = row.get_mut(*col) {
                apply_cast(v, Cast::Bool);
            }
        }
        for (col, cast) in Self::CASTS {
            if let Some(v) = row.get_mut(*col) {
                apply_cast(v, *cast);
            }
        }
    }

    /// Row -> model (applies casts).
    fn from_row(mut row: Row) -> OrmResult<Self> {
        Self::cast_row(&mut row);
        serde_json::from_value(Value::Object(row))
            .map_err(|e| OrmError::Db(format!("row does not fit model for `{}`: {e}", Self::TABLE)))
    }

    /// One attribute of this instance, as JSON.
    fn attribute(&self, col: &str) -> Value {
        serde_json::to_value(self)
            .ok()
            .and_then(|mut v| v.as_object_mut().and_then(|m| m.remove(col)))
            .unwrap_or(Value::Null)
    }

    // ------------------------------------------------------------
    // querying
    // ------------------------------------------------------------

    fn query() -> Builder<Self> {
        Builder::new()
    }

    /// `Post::with(&["comments", "author"])` = `Post::query().with(..)`.
    fn with(relations: &[&str]) -> Builder<Self> {
        Builder::new().with(relations)
    }

    fn with_trashed() -> Builder<Self> {
        Builder::new().with_trashed()
    }

    fn only_trashed() -> Builder<Self> {
        Builder::new().only_trashed()
    }

    fn all() -> OrmResult<Vec<Self>> {
        Self::query().get()
    }

    fn find(id: i64) -> OrmResult<Option<Self>> {
        Self::run(|ex| Self::find_on(ex, id))
    }

    fn find_on(ex: &dyn Executor, id: i64) -> OrmResult<Option<Self>> {
        Self::query().where_eq(Self::PRIMARY_KEY, id).first_on(ex)
    }

    /// Like `find`, but a missing row is an error that becomes a 404.
    fn find_or_fail(id: i64) -> OrmResult<Self> {
        Self::find(id)?.ok_or_else(not_found::<Self>)
    }

    fn find_many(ids: &[i64]) -> OrmResult<Vec<Self>> {
        Self::query()
            .where_in(Self::PRIMARY_KEY, ids.iter().copied())
            .get()
    }

    /// The first row matching every key of `search` (a JSON object).
    fn first_where(search: &Value) -> OrmResult<Option<Self>> {
        Self::query().where_all(search).first()
    }

    // ------------------------------------------------------------
    // writing
    // ------------------------------------------------------------

    /// Keep only the `FILLABLE` keys of `attrs`.
    #[doc(hidden)]
    fn fillable_map<T: Serialize>(attrs: &T) -> OrmResult<Map<String, Value>> {
        Ok(to_object(attrs)?
            .into_iter()
            .filter(|(k, _)| Self::FILLABLE.contains(&k.as_str()))
            .collect())
    }

    /// Insert a new row from the `FILLABLE` subset of `attrs` and return it.
    fn create<T: Serialize>(attrs: &T) -> OrmResult<Self> {
        Self::run(|ex| Self::create_on(ex, attrs))
    }

    fn create_on<T: Serialize>(ex: &dyn Executor, attrs: &T) -> OrmResult<Self> {
        let map = Self::fillable_map(attrs)?;
        Self::insert_map(ex, map)
    }

    /// Insert **all** keys of `attrs`, ignoring `FILLABLE`. Only for data
    /// you built yourself — never for request input.
    fn force_create<T: Serialize>(attrs: &T) -> OrmResult<Self> {
        let map = to_object(attrs)?;
        Self::run(|ex| Self::insert_map(ex, map))
    }

    #[doc(hidden)]
    fn insert_map(ex: &dyn Executor, mut map: Map<String, Value>) -> OrmResult<Self> {
        non_column_keys(&Self::relations(), &mut map);
        if Self::TIMESTAMPS {
            let now = now_iso();
            for k in ["created_at", "updated_at"] {
                if matches!(map.get(k), None | Some(Value::Null)) {
                    map.insert(k.to_string(), Value::String(now.clone()));
                }
            }
        }
        Self::creating(&mut map, ex)?;
        let pairs: Vec<(String, SqlValue)> = map
            .iter()
            .filter(|(k, v)| !(k.as_str() == Self::PRIMARY_KEY && v.is_null()))
            .map(|(k, v)| (k.clone(), SqlValue::from_json(v)))
            .collect();
        let row = Query::new(Self::TABLE).insert_returning_on(ex, pairs)?;
        let model = Self::from_row(row)?;
        model.created(ex)?;
        Ok(model)
    }

    #[doc(hidden)]
    fn find_any_on(ex: &dyn Executor, id: i64) -> OrmResult<Option<Self>> {
        Self::query()
            .without_global_scope()
            .with_trashed()
            .where_eq(Self::PRIMARY_KEY, id)
            .first_on(ex)
    }

    #[doc(hidden)]
    fn write_changes(
        &self,
        ex: &dyn Executor,
        id: i64,
        mut map: Map<String, Value>,
    ) -> OrmResult<Self> {
        map.remove(Self::PRIMARY_KEY);
        non_column_keys(&Self::relations(), &mut map);
        if Self::TIMESTAMPS {
            map.insert("updated_at".into(), Value::String(now_iso()));
        }
        self.updating(&mut map, ex)?;
        if !map.is_empty() {
            let pairs = pairs_from_json(&Value::Object(map))?;
            Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .update_on(ex, pairs)?;
        }
        let fresh = Self::find_any_on(ex, id)?.ok_or_else(not_found::<Self>)?;
        fresh.updated(ex)?;
        Ok(fresh)
    }

    /// INSERT (no id yet) or UPDATE every column (id set). Writes the
    /// struct as it is — this is *your* data, so `FILLABLE` does not apply.
    /// Loaded relations and `*_count` keys are not written.
    fn save(&mut self) -> OrmResult<()> {
        Self::run(|ex| self.save_on(ex))
    }

    fn save_on(&mut self, ex: &dyn Executor) -> OrmResult<()> {
        let map = to_object(&*self)?;
        let fresh = match self.id() {
            None => Self::insert_map(ex, map)?,
            Some(id) => self.write_changes(ex, id, map)?,
        };
        *self = fresh;
        Ok(())
    }

    /// Update this row from the `FILLABLE` subset of `attrs`. Every key you
    /// pass is written (a `null` writes NULL), so pass only what changed.
    fn update<T: Serialize>(&mut self, attrs: &T) -> OrmResult<()> {
        Self::run(|ex| self.update_on(ex, attrs))
    }

    fn update_on<T: Serialize>(&mut self, ex: &dyn Executor, attrs: &T) -> OrmResult<()> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot update an unsaved model".into()))?;
        let map = Self::fillable_map(attrs)?;
        if map.is_empty() {
            return Ok(());
        }
        *self = self.write_changes(ex, id, map)?;
        Ok(())
    }

    /// Find by `search`; otherwise create from `search` + `extra`
    /// (`FILLABLE` applies to both, as in Laravel).
    fn first_or_create(search: &Value, extra: &Value) -> OrmResult<Self> {
        Self::run(|ex| {
            if let Some(found) = Self::query().where_all(search).first_on(ex)? {
                return Ok(found);
            }
            let mut attrs = to_object(search)?;
            if let Value::Object(e) = extra {
                attrs.extend(e.clone());
            }
            Self::create_on(ex, &attrs)
        })
    }

    /// Update the row matching `search` with `values`, or create it.
    fn update_or_create(search: &Value, values: &Value) -> OrmResult<Self> {
        Self::run(|ex| match Self::query().where_all(search).first_on(ex)? {
            Some(mut found) => {
                found.update_on(ex, values)?;
                Ok(found)
            }
            None => {
                let mut attrs = to_object(search)?;
                if let Value::Object(v) = values {
                    attrs.extend(v.clone());
                }
                Self::create_on(ex, &attrs)
            }
        })
    }

    /// Insert many rows (FILLABLE subset, timestamps filled), no hooks.
    fn insert_many(rows: &[Value]) -> OrmResult<u64> {
        let rows = Self::prepare_bulk(rows)?;
        Self::run(|ex| Query::new(Self::TABLE).insert_many_on(ex, &rows))
    }

    /// `INSERT ... ON CONFLICT (unique_by) DO UPDATE SET update...` for many
    /// rows at once. FILLABLE applies (stricter than Laravel), no hooks.
    fn upsert(rows: &[Value], unique_by: &[&str], update: &[&str]) -> OrmResult<u64> {
        let rows = Self::prepare_bulk(rows)?;
        let mut update: Vec<&str> = update.to_vec();
        if Self::TIMESTAMPS && !update.is_empty() && !update.contains(&"updated_at") {
            update.push("updated_at");
        }
        Self::run(|ex| Query::new(Self::TABLE).upsert_on(ex, &rows, unique_by, &update))
    }

    #[doc(hidden)]
    fn prepare_bulk(rows: &[Value]) -> OrmResult<Vec<Value>> {
        let now = now_iso();
        rows.iter()
            .map(|r| {
                let mut m = Self::fillable_map(r)?;
                if Self::TIMESTAMPS {
                    for k in ["created_at", "updated_at"] {
                        m.entry(k).or_insert_with(|| Value::String(now.clone()));
                    }
                }
                Ok(Value::Object(m))
            })
            .collect()
    }

    /// Atomic `col = col + by` on this row, then reload.
    fn increment(&mut self, col: &str, by: i64) -> OrmResult<()> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot increment an unsaved model".into()))?;
        Self::run(|ex| {
            let extra = if Self::TIMESTAMPS {
                vec![("updated_at".to_string(), SqlValue::Text(now_iso()))]
            } else {
                Vec::new()
            };
            Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .increment_on(ex, col, by, extra)?;
            *self = Self::find_any_on(ex, id)?.ok_or_else(not_found::<Self>)?;
            Ok(())
        })
    }

    fn decrement(&mut self, col: &str, by: i64) -> OrmResult<()> {
        self.increment(col, by.saturating_neg())
    }

    /// Bump `updated_at`.
    fn touch(&mut self) -> OrmResult<()> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot touch an unsaved model".into()))?;
        Self::run(|ex| {
            Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .update_on(ex, vec![("updated_at".into(), SqlValue::Text(now_iso()))])?;
            *self = Self::find_any_on(ex, id)?.ok_or_else(not_found::<Self>)?;
            Ok(())
        })
    }

    /// An unsaved copy (no id, no timestamps, no loaded relations).
    fn replicate(&self) -> OrmResult<Self> {
        let mut m = to_object(self)?;
        m.insert(Self::PRIMARY_KEY.to_string(), Value::Null);
        for k in ["created_at", "updated_at", "deleted_at"] {
            if m.contains_key(k) {
                m.insert(k.to_string(), Value::Null);
            }
        }
        non_column_keys(&Self::relations(), &mut m);
        serde_json::from_value(Value::Object(m))
            .map_err(|e| OrmError::Invalid(format!("cannot replicate `{}`: {e}", Self::TABLE)))
    }

    /// Delete this row (sets `deleted_at` when `SOFT_DELETES`).
    fn delete(&self) -> OrmResult<u64> {
        Self::run(|ex| self.delete_on(ex))
    }

    fn delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot delete an unsaved model".into()))?;
        self.deleting(ex)?;
        let q = Query::new(Self::TABLE).where_eq(Self::PRIMARY_KEY, id);
        let n = if Self::SOFT_DELETES {
            q.update_on(ex, vec![("deleted_at".into(), SqlValue::Text(now_iso()))])?
        } else {
            q.delete_on(ex)?
        };
        self.deleted(ex)?;
        Ok(n)
    }

    /// Delete by primary keys (runs the hooks for each row).
    fn destroy(ids: &[i64]) -> OrmResult<u64> {
        Self::run(|ex| {
            let mut n = 0;
            for m in Self::query()
                .where_in(Self::PRIMARY_KEY, ids.iter().copied())
                .get_on(ex)?
            {
                n += m.delete_on(ex)?;
            }
            Ok(n)
        })
    }

    /// Really delete the row, even with `SOFT_DELETES`.
    fn force_delete(&self) -> OrmResult<u64> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot delete an unsaved model".into()))?;
        Self::run(|ex| {
            self.deleting(ex)?;
            let n = Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .delete_on(ex)?;
            self.deleted(ex)?;
            Ok(n)
        })
    }

    /// Undo a soft delete.
    fn restore(&mut self) -> OrmResult<()> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot restore an unsaved model".into()))?;
        Self::run(|ex| {
            Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .update_on(ex, vec![("deleted_at".into(), SqlValue::Null)])?;
            *self = Self::find_any_on(ex, id)?.ok_or_else(not_found::<Self>)?;
            Ok(())
        })
    }

    /// Reload from the database.
    fn fresh(&self) -> OrmResult<Option<Self>> {
        match self.id() {
            Some(id) => Self::run(|ex| Self::find_any_on(ex, id)),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------
    // relations
    // ------------------------------------------------------------

    /// Eager-load relations onto an already fetched instance.
    fn load(&mut self, relations: &[&str]) -> OrmResult<()> {
        let mut rows = vec![to_object(&*self)?];
        let with: Vec<With> = relations
            .iter()
            .map(|p| With {
                path: p.to_string(),
                constraint: None,
            })
            .collect();
        Self::run(|ex| eager_load(ex, &mut rows, &Self::relations(), Self::TABLE, &with))?;
        let row = rows
            .pop()
            .ok_or_else(|| OrmError::Db("load lost the model".into()))?;
        *self = Self::from_row(row)?;
        Ok(())
    }

    /// Eager-load relations onto a list of models (one query per relation).
    fn load_many(items: &mut Vec<Self>, relations: &[&str]) -> OrmResult<()> {
        let mut rows = items.iter().map(to_object).collect::<OrmResult<Vec<_>>>()?;
        let with: Vec<With> = relations
            .iter()
            .map(|p| With {
                path: p.to_string(),
                constraint: None,
            })
            .collect();
        Self::run(|ex| eager_load(ex, &mut rows, &Self::relations(), Self::TABLE, &with))?;
        *items = rows
            .into_iter()
            .map(Self::from_row)
            .collect::<OrmResult<Vec<_>>>()?;
        Ok(())
    }

    /// A query for the rows of relation `name` that belong to this model:
    /// `post.related::<Comment>("comments").where_eq("approved", true).get()`.
    fn related<R: Model>(&self, name: &str) -> Builder<R> {
        let mut b = R::query();
        let rel = match find_relation(&Self::relations(), name, Self::TABLE) {
            Ok(r) => r,
            Err(e) => {
                b.q.fail(e);
                return b;
            }
        };
        if rel.meta.table != R::TABLE {
            b.q.fail(OrmError::Invalid(format!(
                "relation `{name}` is not of `{}`",
                R::TABLE
            )));
            return b;
        }
        let key = SqlValue::from_json(&self.attribute(rel.parent_key()));
        if key == SqlValue::Null {
            return b.where_raw("1 = 0", Vec::new());
        }
        match rel.kind {
            Kind::HasMany { foreign_key, .. } | Kind::HasOne { foreign_key, .. } => {
                b.where_eq(foreign_key, key)
            }
            Kind::BelongsTo { owner_key, .. } => b.where_eq(owner_key, key),
            Kind::BelongsToMany {
                pivot,
                foreign_pivot_key,
                related_pivot_key,
                related_key,
                ..
            } => b.where_in_sub(
                related_key,
                Query::new(pivot)
                    .select([related_pivot_key])
                    .where_eq(foreign_pivot_key, key),
            ),
            Kind::HasManyThrough {
                through,
                first_key,
                second_key,
                through_key,
                ..
            } => b.where_in_sub(
                second_key,
                Query::new(through)
                    .select([through_key])
                    .where_eq(first_key, key),
            ),
        }
    }

    /// `post.has_many::<Comment>("post_id")` -> comments whose `post_id`
    /// is this model's id (ad-hoc, without a `Relation` definition).
    fn has_many<R: Model>(&self, foreign_key: &str) -> Builder<R> {
        match self.id() {
            Some(id) => R::query().where_eq(foreign_key, id),
            None => R::query().where_raw("1 = 0", Vec::new()),
        }
    }

    fn has_one<R: Model>(&self, foreign_key: &str) -> OrmResult<Option<R>> {
        self.has_many::<R>(foreign_key).first()
    }

    /// `comment.belongs_to::<Post>(Some(comment.post_id))`.
    fn belongs_to<R: Model>(&self, foreign_key_value: Option<i64>) -> OrmResult<Option<R>> {
        match foreign_key_value {
            Some(id) => R::find(id),
            None => Ok(None),
        }
    }

    // ---------- many-to-many pivot operations ----------

    #[doc(hidden)]
    fn pivot_info(&self, name: &str) -> OrmResult<(Relation, SqlValue)> {
        let rel = find_relation(&Self::relations(), name, Self::TABLE)?;
        if !matches!(rel.kind, Kind::BelongsToMany { .. }) {
            return Err(OrmError::Invalid(format!(
                "`{name}` is not a belongs_to_many relation"
            )));
        }
        let key = SqlValue::from_json(&self.attribute(rel.parent_key()));
        if key == SqlValue::Null {
            return Err(OrmError::Invalid(
                "save the model before touching its pivot".into(),
            ));
        }
        Ok((rel, key))
    }

    /// Attach related ids (already attached ids are skipped).
    fn attach(&self, name: &str, ids: &[i64]) -> OrmResult<u64> {
        self.attach_with(name, ids, &json!({}))
    }

    /// Attach with extra pivot columns: `attach_with("tags", &[1], &json!({"role": "owner"}))`.
    fn attach_with(&self, name: &str, ids: &[i64], pivot_data: &Value) -> OrmResult<u64> {
        let (rel, key) = self.pivot_info(name)?;
        let Kind::BelongsToMany {
            pivot,
            foreign_pivot_key,
            related_pivot_key,
            pivot_timestamps,
            ..
        } = rel.kind
        else {
            unreachable!()
        };
        let extra = match pivot_data {
            Value::Object(m) => m.clone(),
            _ => return Err(OrmError::Invalid("pivot data must be an object".into())),
        };
        Self::run(|ex| {
            let existing: Vec<i64> = Query::new(pivot)
                .where_eq(foreign_pivot_key, key.clone())
                .where_in(related_pivot_key, ids.iter().copied())
                .pluck_on(ex, related_pivot_key)?
                .iter()
                .filter_map(|v| json_as_f64(v).map(|f| f as i64))
                .collect();
            let now = now_iso();
            let mut rows = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for id in ids {
                if existing.contains(id) || !seen.insert(*id) {
                    continue;
                }
                let mut m = extra.clone();
                m.insert(foreign_pivot_key.to_string(), sql_to_json(&key));
                m.insert(related_pivot_key.to_string(), Value::from(*id));
                if pivot_timestamps {
                    m.insert("created_at".into(), Value::String(now.clone()));
                    m.insert("updated_at".into(), Value::String(now.clone()));
                }
                rows.push(Value::Object(m));
            }
            Query::new(pivot).insert_many_on(ex, &rows)
        })
    }

    /// Detach related ids; an empty slice detaches everything.
    fn detach(&self, name: &str, ids: &[i64]) -> OrmResult<u64> {
        let (rel, key) = self.pivot_info(name)?;
        let Kind::BelongsToMany {
            pivot,
            foreign_pivot_key,
            related_pivot_key,
            ..
        } = rel.kind
        else {
            unreachable!()
        };
        Self::run(|ex| {
            let q = Query::new(pivot).where_eq(foreign_pivot_key, key);
            let q = if ids.is_empty() {
                q
            } else {
                q.where_in(related_pivot_key, ids.iter().copied())
            };
            q.delete_on(ex)
        })
    }

    /// Make the attached set exactly `ids` (atomic).
    fn sync(&self, name: &str, ids: &[i64]) -> OrmResult<SyncResult> {
        let (rel, key) = self.pivot_info(name)?;
        let Kind::BelongsToMany {
            pivot,
            foreign_pivot_key,
            related_pivot_key,
            pivot_timestamps,
            ..
        } = rel.kind
        else {
            unreachable!()
        };
        let conn = crate::conn::Db::connection(Self::CONNECTION)?;
        transaction_in(&*conn, |tx| {
            let current: Vec<i64> = Query::new(pivot)
                .where_eq(foreign_pivot_key, key.clone())
                .pluck_on(tx, related_pivot_key)?
                .iter()
                .filter_map(|v| json_as_f64(v).map(|f| f as i64))
                .collect();
            let detached: Vec<i64> = current
                .iter()
                .copied()
                .filter(|c| !ids.contains(c))
                .collect();
            let mut attached: Vec<i64> = Vec::new();
            for id in ids {
                if !current.contains(id) && !attached.contains(id) {
                    attached.push(*id);
                }
            }
            if !detached.is_empty() {
                Query::new(pivot)
                    .where_eq(foreign_pivot_key, key.clone())
                    .where_in(related_pivot_key, detached.iter().copied())
                    .delete_on(tx)?;
            }
            let now = now_iso();
            let rows: Vec<Value> = attached
                .iter()
                .map(|id| {
                    let mut m = Map::new();
                    m.insert(foreign_pivot_key.to_string(), sql_to_json(&key));
                    m.insert(related_pivot_key.to_string(), Value::from(*id));
                    if pivot_timestamps {
                        m.insert("created_at".into(), Value::String(now.clone()));
                        m.insert("updated_at".into(), Value::String(now.clone()));
                    }
                    Value::Object(m)
                })
                .collect();
            Query::new(pivot).insert_many_on(tx, &rows)?;
            Ok(SyncResult { attached, detached })
        })
    }

    // ------------------------------------------------------------
    // output
    // ------------------------------------------------------------

    /// JSON for clients: the model without its `HIDDEN` columns (also
    /// stripped from loaded relations, recursively).
    fn public_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        strip_hidden(&mut v, Self::HIDDEN, &Self::relations());
        v
    }

    /// `public_json` as a response.
    fn json_response(&self, status: Status) -> Response {
        Response::json(status, &self.public_json())
    }
}

fn sql_to_json(v: &SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Bool(b) => Value::Bool(*b),
        SqlValue::Int(i) => Value::from(*i),
        SqlValue::Float(f) => Value::from(*f),
        SqlValue::Text(s) => Value::String(s.clone()),
    }
}

/// `{"data": [...]}` without hidden columns.
pub fn collection_json<M: Model>(items: &[M]) -> Value {
    json!({ "data": items.iter().map(|m| m.public_json()).collect::<Vec<_>>() })
}

// ----------------------------------------------------------------------
// typed builder
// ----------------------------------------------------------------------

pub struct Builder<M: Model> {
    pub(crate) q: Query,
    with: Vec<With>,
    scoped: bool,
    exists_cols: Vec<String>,
    _m: PhantomData<fn() -> M>,
}

impl<M: Model> Clone for Builder<M> {
    fn clone(&self) -> Self {
        Builder {
            q: self.q.clone(),
            with: self.with.clone(),
            scoped: self.scoped,
            exists_cols: self.exists_cols.clone(),
            _m: PhantomData,
        }
    }
}

impl<M: Model> Default for Builder<M> {
    fn default() -> Self {
        Self::new()
    }
}

macro_rules! forward {
    ($( $(#[$doc:meta])* fn $name:ident(self $(, $arg:ident : $ty:ty)*); )*) => {$(
        $(#[$doc])*
        pub fn $name(self $(, $arg: $ty)*) -> Self {
            self.map(|q| q.$name($($arg),*))
        }
    )*};
}

impl<M: Model> Builder<M> {
    pub fn new() -> Self {
        let mut q = Query::new(M::TABLE);
        if M::SOFT_DELETES {
            q = q.soft_deletes("deleted_at");
        }
        if let Some(c) = M::CONNECTION {
            q = q.on(c);
        }
        Builder {
            q,
            with: Vec::new(),
            scoped: true,
            exists_cols: Vec::new(),
            _m: PhantomData,
        }
    }

    fn map(mut self, f: impl FnOnce(Query) -> Query) -> Self {
        self.q = f(self.q);
        self
    }

    /// Escape hatch: edit the underlying untyped query.
    pub fn query(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.map(f)
    }

    /// The query as it will run (global scope applied).
    fn built(&self) -> Query {
        if self.scoped {
            self.q.clone().with_scope(M::global_scope)
        } else {
            self.q.clone()
        }
    }

    pub fn without_global_scope(mut self) -> Self {
        self.scoped = false;
        self
    }

    /// `when(cond, |b| ...)`: apply only if `cond`.
    pub fn when(self, cond: bool, f: impl FnOnce(Self) -> Self) -> Self {
        if cond { f(self) } else { self }
    }

    /// Apply a local scope: `Post::query().scope(published)` with
    /// `fn published(b: Builder<Post>) -> Builder<Post>`.
    pub fn scope(self, f: impl FnOnce(Self) -> Self) -> Self {
        f(self)
    }

    forward! {
        fn where_(self, col: &str, op: &str, val: impl Into<SqlValue>);
        fn where_eq(self, col: &str, val: impl Into<SqlValue>);
        fn or_where(self, col: &str, op: &str, val: impl Into<SqlValue>);
        fn or_where_eq(self, col: &str, val: impl Into<SqlValue>);
        fn where_all(self, pairs: &Value);
        fn where_like(self, col: &str, pattern: impl Into<SqlValue>);
        fn where_column(self, a: &str, op: &str, b: &str);
        fn where_null(self, col: &str);
        fn where_not_null(self, col: &str);
        fn or_where_null(self, col: &str);
        fn or_where_not_null(self, col: &str);
        fn where_between(self, col: &str, lo: impl Into<SqlValue>, hi: impl Into<SqlValue>);
        fn where_not_between(self, col: &str, lo: impl Into<SqlValue>, hi: impl Into<SqlValue>);
        fn where_date(self, col: &str, op: &str, date: impl Into<SqlValue>);
        fn where_json(self, path: &str, op: &str, val: impl Into<SqlValue>);
        fn where_raw(self, sql: &'static str, params: Vec<SqlValue>);
        fn or_where_raw(self, sql: &'static str, params: Vec<SqlValue>);
        fn where_in_sub(self, col: &str, sub: Query);
        fn where_not_in_sub(self, col: &str, sub: Query);
        fn where_exists(self, sub: Query);
        fn where_not_exists(self, sub: Query);
        fn join(self, table: &str, a: &str, op: &str, b: &str);
        fn left_join(self, table: &str, a: &str, op: &str, b: &str);
        fn order_by(self, col: &str, direction: &str);
        fn order_by_desc(self, col: &str);
        fn order_by_raw(self, sql: &'static str);
        fn latest(self);
        fn oldest(self);
        fn reorder(self);
        fn sort_by(self, requested: Option<&str>, allowed: &[&str], default: (&str, &str));
        fn limit(self, n: u64);
        fn take(self, n: u64);
        fn offset(self, n: u64);
        fn skip(self, n: u64);
        fn distinct(self);
        fn lock_for_update(self);
        fn shared_lock(self);
        fn with_trashed(self);
        fn only_trashed(self);
        fn all_rows(self);
    }

    pub fn where_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.map(|q| q.where_in(col, vals))
    }
    pub fn or_where_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.map(|q| q.or_where_in(col, vals))
    }
    pub fn where_not_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.map(|q| q.where_not_in(col, vals))
    }
    pub fn select<I, S>(self, cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.map(|q| q.select(cols))
    }
    pub fn where_group(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.map(|q| q.where_group(f))
    }
    pub fn or_where_group(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.map(|q| q.or_where_group(f))
    }

    // ---------- relations ----------

    /// Eager-load relations: `with(&["author", "comments.author"])`.
    /// One query per relation level, whatever the number of rows.
    pub fn with(mut self, relations: &[&str]) -> Self {
        for r in relations {
            self.with.push(With {
                path: r.to_string(),
                constraint: None,
            });
        }
        self
    }

    /// Eager-load with a constraint on the (last segment of the) relation:
    /// `with_where("comments", |q| q.where_eq("approved", true).latest())`.
    pub fn with_where(
        mut self,
        relation: &str,
        f: impl Fn(Query) -> Query + Send + Sync + 'static,
    ) -> Self {
        let c: Constraint = Arc::new(f);
        self.with.push(With {
            path: relation.to_string(),
            constraint: Some(c),
        });
        self
    }

    fn has_impl(mut self, name: &str, c: Option<Constraint>, negate: bool, j: Joiner) -> Self {
        self.q = add_has(self.q, &M::relations(), M::TABLE, name, c, negate, j);
        self
    }

    /// Rows that have at least one related row.
    pub fn has(self, relation: &str) -> Self {
        self.has_impl(relation, None, false, Joiner::And)
    }

    pub fn doesnt_have(self, relation: &str) -> Self {
        self.has_impl(relation, None, true, Joiner::And)
    }

    /// Rows with a related row matching `f`:
    /// `where_has("comments", |q| q.where_eq("approved", true))`.
    pub fn where_has(
        self,
        relation: &str,
        f: impl Fn(Query) -> Query + Send + Sync + 'static,
    ) -> Self {
        self.has_impl(relation, Some(Arc::new(f)), false, Joiner::And)
    }

    pub fn or_where_has(
        self,
        relation: &str,
        f: impl Fn(Query) -> Query + Send + Sync + 'static,
    ) -> Self {
        self.has_impl(relation, Some(Arc::new(f)), false, Joiner::Or)
    }

    pub fn where_doesnt_have(
        self,
        relation: &str,
        f: impl Fn(Query) -> Query + Send + Sync + 'static,
    ) -> Self {
        self.has_impl(relation, Some(Arc::new(f)), true, Joiner::And)
    }

    /// Add `<relation>_count` to every row (one SQL statement, no N+1).
    pub fn with_count(mut self, relations: &[&str]) -> Self {
        for r in relations {
            self.q = add_aggregate(self.q, &M::relations(), M::TABLE, r, Agg::Count, None, None);
        }
        self
    }

    /// `<relation>_count` of related rows matching `f`.
    pub fn with_count_where(
        mut self,
        relation: &str,
        f: impl Fn(Query) -> Query + Send + Sync + 'static,
    ) -> Self {
        self.q = add_aggregate(
            self.q,
            &M::relations(),
            M::TABLE,
            relation,
            Agg::Count,
            None,
            Some(Arc::new(f)),
        );
        self
    }

    /// `<relation>_sum_<col>` (also `with_avg`, `with_min`, `with_max`).
    pub fn with_sum(self, relation: &str, col: &str) -> Self {
        self.with_aggregate(relation, Agg::Sum, col)
    }
    pub fn with_avg(self, relation: &str, col: &str) -> Self {
        self.with_aggregate(relation, Agg::Avg, col)
    }
    pub fn with_min(self, relation: &str, col: &str) -> Self {
        self.with_aggregate(relation, Agg::Min, col)
    }
    pub fn with_max(self, relation: &str, col: &str) -> Self {
        self.with_aggregate(relation, Agg::Max, col)
    }
    /// `<relation>_exists` (a boolean) for each relation.
    pub fn with_exists(mut self, relations: &[&str]) -> Self {
        for r in relations {
            self.q = add_exists_select(self.q, &M::relations(), M::TABLE, r);
            self.exists_cols.push(format!("{r}_exists"));
        }
        self
    }

    fn with_aggregate(mut self, relation: &str, agg: Agg, col: &str) -> Self {
        self.q = add_aggregate(
            self.q,
            &M::relations(),
            M::TABLE,
            relation,
            agg,
            Some(col),
            None,
        );
        self
    }

    /// The generated SQL and bound values (debugging, tests; SQLite flavour).
    pub fn to_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.built().to_select_sql()
    }

    // ---------- running ----------

    fn hydrate(&self, ex: &dyn Executor, mut rows: Vec<Row>) -> OrmResult<Vec<M>> {
        // SQLite returns EXISTS as 0/1, PostgreSQL as a boolean: normalise.
        for row in &mut rows {
            for c in &self.exists_cols {
                if let Some(v) = row.get_mut(c) {
                    let b = match v {
                        Value::Bool(b) => *b,
                        Value::Number(n) => n.as_i64().unwrap_or(0) != 0,
                        _ => false,
                    };
                    *v = Value::Bool(b);
                }
            }
        }
        eager_load(ex, &mut rows, &M::relations(), M::TABLE, &self.with)?;
        rows.into_iter().map(M::from_row).collect()
    }

    pub fn get_on(&self, ex: &dyn Executor) -> OrmResult<Vec<M>> {
        let rows = self.built().get_on(ex)?;
        self.hydrate(ex, rows)
    }

    pub fn first_on(&self, ex: &dyn Executor) -> OrmResult<Option<M>> {
        let rows = self.built().limit(1).get_on(ex)?;
        Ok(self.hydrate(ex, rows)?.into_iter().next())
    }

    pub fn count_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        self.built().count_on(ex)
    }

    pub fn exists_on(&self, ex: &dyn Executor) -> OrmResult<bool> {
        self.built().exists_on(ex)
    }

    /// `page` starts at 1. `per_page` is clamped to `1..=MAX_PER_PAGE`.
    pub fn paginate_on(&self, ex: &dyn Executor, page: u64, per_page: u64) -> OrmResult<Page<M>> {
        let per_page = per_page.clamp(1, MAX_PER_PAGE);
        let page = page.max(1);
        let q = self.built();
        let total = q.count_on(ex)?;
        let rows = q
            .limit(per_page)
            .offset((page - 1).saturating_mul(per_page))
            .get_on(ex)?;
        let data = self.hydrate(ex, rows)?;
        let last_page = if total == 0 {
            1
        } else {
            total.div_ceil(per_page)
        };
        Ok(Page {
            data,
            total,
            page,
            per_page,
            last_page,
        })
    }

    /// Like `paginate` without the COUNT query (fast "next page" lists).
    pub fn simple_paginate_on(
        &self,
        ex: &dyn Executor,
        page: u64,
        per_page: u64,
    ) -> OrmResult<SimplePage<M>> {
        let per_page = per_page.clamp(1, MAX_PER_PAGE);
        let page = page.max(1);
        let mut rows = self
            .built()
            .limit(per_page + 1)
            .offset((page - 1).saturating_mul(per_page))
            .get_on(ex)?;
        let has_more = rows.len() as u64 > per_page;
        rows.truncate(per_page as usize);
        Ok(SimplePage {
            data: self.hydrate(ex, rows)?,
            page,
            per_page,
            has_more,
        })
    }

    /// Keyset ("cursor") pagination on the primary key: stable while rows
    /// are inserted, and as fast on page 10 000 as on page 1. Pass the
    /// `next_cursor` of the previous page (from the client) as `cursor`;
    /// a malformed cursor is a 400.
    pub fn cursor_paginate_on(
        &self,
        ex: &dyn Executor,
        per_page: u64,
        cursor: Option<&str>,
        descending: bool,
    ) -> OrmResult<CursorPage<M>> {
        let per_page = per_page.clamp(1, MAX_PER_PAGE);
        let pk = M::PRIMARY_KEY;
        let mut q = self
            .built()
            .reorder()
            .order_by(pk, if descending { "desc" } else { "asc" });
        if let Some(c) = cursor.filter(|c| !c.is_empty()) {
            let after =
                decode_cursor(c).ok_or_else(|| OrmError::Invalid("malformed cursor".into()))?;
            q = q.where_(pk, if descending { "<" } else { ">" }, after);
        }
        let mut rows = q.limit(per_page + 1).get_on(ex)?;
        let has_more = rows.len() as u64 > per_page;
        rows.truncate(per_page as usize);
        let next_cursor = if has_more {
            rows.last()
                .and_then(|r| r.get(pk))
                .and_then(Value::as_i64)
                .map(encode_cursor)
        } else {
            None
        };
        Ok(CursorPage {
            data: self.hydrate(ex, rows)?,
            per_page,
            next_cursor,
        })
    }

    pub fn pluck_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Vec<Value>> {
        self.built().pluck_on(ex, col)
    }
    pub fn value_on(&self, ex: &dyn Executor, col: &str) -> OrmResult<Option<Value>> {
        self.built().value_on(ex, col)
    }

    /// Process every matching model in primary-key batches (no OFFSET).
    pub fn chunk_by_id_on(
        &self,
        ex: &dyn Executor,
        size: u64,
        mut f: impl FnMut(Vec<M>) -> OrmResult<bool>,
    ) -> OrmResult<()> {
        self.built()
            .chunk_by_id_on(ex, size, M::PRIMARY_KEY, |rows| f(self.hydrate(ex, rows)?))
    }

    /// Bulk-update the matching rows from the `FILLABLE` subset of `attrs`.
    /// No model hooks run (same as Laravel). Returns the rows changed.
    pub fn update_on<T: Serialize>(&self, ex: &dyn Executor, attrs: &T) -> OrmResult<u64> {
        let mut map = M::fillable_map(attrs)?;
        if map.is_empty() {
            return Ok(0);
        }
        if M::TIMESTAMPS {
            map.insert("updated_at".into(), Value::String(now_iso()));
        }
        self.built()
            .update_on(ex, pairs_from_json(&Value::Object(map))?)
    }

    /// Atomic `col = col + by` on every matching row.
    pub fn increment_on(&self, ex: &dyn Executor, col: &str, by: i64) -> OrmResult<u64> {
        let extra = if M::TIMESTAMPS {
            vec![("updated_at".to_string(), SqlValue::Text(now_iso()))]
        } else {
            Vec::new()
        };
        self.built().increment_on(ex, col, by, extra)
    }

    /// Bulk delete (soft when the model uses soft deletes). No hooks.
    pub fn delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let q = self.built();
        if M::SOFT_DELETES {
            q.update_on(ex, vec![("deleted_at".into(), SqlValue::Text(now_iso()))])
        } else {
            q.delete_on(ex)
        }
    }

    /// Permanently delete matching rows, even with soft deletes.
    pub fn force_delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        self.built().delete_on(ex)
    }

    // ---------- the same, on the model's connection ----------

    pub fn get(&self) -> OrmResult<Vec<M>> {
        M::run(|ex| self.get_on(ex))
    }
    pub fn first(&self) -> OrmResult<Option<M>> {
        M::run(|ex| self.first_on(ex))
    }
    /// Like `first`, but a miss is a 404.
    pub fn first_or_fail(&self) -> OrmResult<M> {
        self.first()?.ok_or_else(not_found::<M>)
    }
    /// Exactly one match, else NotFound (0) / Invalid (more than one).
    pub fn sole(&self) -> OrmResult<M> {
        let mut v = M::run(|ex| self.clone().limit(2).get_on(ex))?;
        match v.len() {
            0 => Err(not_found::<M>()),
            1 => Ok(v.remove(0)),
            _ => Err(OrmError::Invalid(format!(
                "more than one `{}` matched",
                M::TABLE
            ))),
        }
    }
    pub fn count(&self) -> OrmResult<u64> {
        M::run(|ex| self.count_on(ex))
    }
    pub fn exists(&self) -> OrmResult<bool> {
        M::run(|ex| self.exists_on(ex))
    }
    pub fn doesnt_exist(&self) -> OrmResult<bool> {
        Ok(!self.exists()?)
    }
    pub fn sum(&self, col: &str) -> OrmResult<f64> {
        M::run(|ex| self.built().sum_on(ex, col))
    }
    pub fn avg(&self, col: &str) -> OrmResult<Option<f64>> {
        M::run(|ex| self.built().avg_on(ex, col))
    }
    pub fn min(&self, col: &str) -> OrmResult<Value> {
        M::run(|ex| self.built().min_on(ex, col))
    }
    pub fn max(&self, col: &str) -> OrmResult<Value> {
        M::run(|ex| self.built().max_on(ex, col))
    }
    pub fn pluck(&self, col: &str) -> OrmResult<Vec<Value>> {
        M::run(|ex| self.pluck_on(ex, col))
    }
    pub fn value(&self, col: &str) -> OrmResult<Option<Value>> {
        M::run(|ex| self.value_on(ex, col))
    }
    pub fn paginate(&self, page: u64, per_page: u64) -> OrmResult<Page<M>> {
        M::run(|ex| self.paginate_on(ex, page, per_page))
    }
    pub fn simple_paginate(&self, page: u64, per_page: u64) -> OrmResult<SimplePage<M>> {
        M::run(|ex| self.simple_paginate_on(ex, page, per_page))
    }
    pub fn cursor_paginate(&self, per_page: u64, cursor: Option<&str>) -> OrmResult<CursorPage<M>> {
        M::run(|ex| self.cursor_paginate_on(ex, per_page, cursor, false))
    }
    pub fn cursor_paginate_desc(
        &self,
        per_page: u64,
        cursor: Option<&str>,
    ) -> OrmResult<CursorPage<M>> {
        M::run(|ex| self.cursor_paginate_on(ex, per_page, cursor, true))
    }
    pub fn chunk_by_id(
        &self,
        size: u64,
        f: impl FnMut(Vec<M>) -> OrmResult<bool>,
    ) -> OrmResult<()> {
        M::run(|ex| self.chunk_by_id_on(ex, size, f))
    }
    pub fn update<T: Serialize>(&self, attrs: &T) -> OrmResult<u64> {
        M::run(|ex| self.update_on(ex, attrs))
    }
    pub fn increment(&self, col: &str, by: i64) -> OrmResult<u64> {
        M::run(|ex| self.increment_on(ex, col, by))
    }
    pub fn decrement(&self, col: &str, by: i64) -> OrmResult<u64> {
        M::run(|ex| self.increment_on(ex, col, by.saturating_neg()))
    }
    pub fn delete(&self) -> OrmResult<u64> {
        M::run(|ex| self.delete_on(ex))
    }
    pub fn force_delete(&self) -> OrmResult<u64> {
        M::run(|ex| self.force_delete_on(ex))
    }
}

// ----------------------------------------------------------------------
// pages
// ----------------------------------------------------------------------

/// One page of results.
pub struct Page<M: Model> {
    pub data: Vec<M>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub last_page: u64,
}

impl<M: Model> Page<M> {
    /// `{"data": [...], "meta": {"total", "page", "per_page", "last_page"}}`
    /// with hidden columns removed.
    pub fn to_json(&self) -> Value {
        json!({
            "data": self.data.iter().map(|m| m.public_json()).collect::<Vec<_>>(),
            "meta": {
                "total": self.total,
                "page": self.page,
                "per_page": self.per_page,
                "last_page": self.last_page,
            }
        })
    }

    pub fn json_response(&self) -> Response {
        Response::json(Status::OK, &self.to_json())
    }
}

/// A page without a total (`simple_paginate`).
pub struct SimplePage<M: Model> {
    pub data: Vec<M>,
    pub page: u64,
    pub per_page: u64,
    pub has_more: bool,
}

impl<M: Model> SimplePage<M> {
    pub fn to_json(&self) -> Value {
        json!({
            "data": self.data.iter().map(|m| m.public_json()).collect::<Vec<_>>(),
            "meta": { "page": self.page, "per_page": self.per_page, "has_more": self.has_more }
        })
    }
    pub fn json_response(&self) -> Response {
        Response::json(Status::OK, &self.to_json())
    }
}

/// A keyset page (`cursor_paginate`).
pub struct CursorPage<M: Model> {
    pub data: Vec<M>,
    pub per_page: u64,
    /// Send this back as `?cursor=` for the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

impl<M: Model> CursorPage<M> {
    pub fn to_json(&self) -> Value {
        json!({
            "data": self.data.iter().map(|m| m.public_json()).collect::<Vec<_>>(),
            "meta": { "per_page": self.per_page, "next_cursor": self.next_cursor }
        })
    }
    pub fn json_response(&self) -> Response {
        Response::json(Status::OK, &self.to_json())
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Opaque-looking, URL-safe cursor. It only ever decodes to an integer, so
/// a tampered cursor can at worst jump to another page.
fn encode_cursor(id: i64) -> String {
    let bytes = format!("k:{id}").into_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn decode_cursor(s: &str) -> Option<i64> {
    if s.len() > 32 {
        return None;
    }
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut bytes = Vec::new();
    for c in s.bytes() {
        let v = B64.iter().position(|b| *b == c)? as u32;
        bits = (bits << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            bytes.push(((bits >> nbits) & 0xFF) as u8);
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    text.strip_prefix("k:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip_and_tamper() {
        for id in [0, 1, 42, 999_999, i64::MAX, -5] {
            assert_eq!(decode_cursor(&encode_cursor(id)), Some(id));
        }
        assert_eq!(decode_cursor("not a cursor!"), None);
        assert_eq!(decode_cursor(""), None);
        assert_eq!(decode_cursor(&"A".repeat(100)), None);
    }

    #[test]
    fn casts() {
        let mut v = Value::from(1);
        apply_cast(&mut v, Cast::Bool);
        assert_eq!(v, Value::Bool(true));
        let mut v = Value::from("{\"a\":1}");
        apply_cast(&mut v, Cast::Json);
        assert_eq!(v["a"], 1);
        let mut v = Value::from("19.99");
        apply_cast(&mut v, Cast::Float);
        assert_eq!(v, Value::from(19.99));
        let mut v = Value::from(12.5);
        apply_cast(&mut v, Cast::String);
        assert_eq!(v, Value::from("12.5"));
        let mut v = Value::from("nope");
        apply_cast(&mut v, Cast::Int);
        assert_eq!(v, Value::from("nope"), "unparseable values are left alone");
    }
}
