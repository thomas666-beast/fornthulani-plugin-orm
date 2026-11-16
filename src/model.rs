//! Eloquent-style models.
//!
//! ```ignore
//! #[derive(Serialize, Deserialize)]
//! struct Post {
//!     id: Option<i64>,
//!     title: String,
//!     body: String,
//!     published: bool,
//!     created_at: Option<String>,
//!     updated_at: Option<String>,
//! }
//!
//! impl Model for Post {
//!     const TABLE: &'static str = "posts";
//!     const FILLABLE: &'static [&'static str] = &["title", "body", "published"];
//!     const BOOLS: &'static [&'static str] = &["published"];
//!     fn id(&self) -> Option<i64> { self.id }
//! }
//!
//! let post = Post::create(&json!({"title": "Hi", "body": "..."}))?;
//! let posts = Post::query().where_eq("published", true).latest().paginate(1, 20)?;
//! ```

use crate::conn::{Executor, with_db};
use crate::error::{OrmError, OrmResult};
use crate::query::{Query, pairs_from_json};
use crate::time::now_iso;
use crate::value::{Row, SqlValue};
use fornthulani::{Response, Status};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use std::marker::PhantomData;

/// Hard cap for `paginate`, so `?per_page=1000000` cannot dump a table.
pub const MAX_PER_PAGE: u64 = 100;

pub trait Model: Serialize + DeserializeOwned + Sized {
    const TABLE: &'static str;
    const PRIMARY_KEY: &'static str = "id";
    /// Maintain `created_at` / `updated_at` (ISO-8601 UTC text).
    const TIMESTAMPS: bool = true;
    /// Use a `deleted_at` column instead of really deleting.
    const SOFT_DELETES: bool = false;
    /// Mass-assignment allow-list for `create` / `update`. **Empty by
    /// default: nothing is fillable until you say so**, so a client can
    /// never set `is_admin` just by sending it.
    const FILLABLE: &'static [&'static str] = &[];
    /// Columns removed by `public_json` (passwords, tokens, ...).
    const HIDDEN: &'static [&'static str] = &[];
    /// `bool` columns (SQLite stores 0/1).
    const BOOLS: &'static [&'static str] = &[];

    /// The primary key of this instance (`None` before it is saved).
    fn id(&self) -> Option<i64>;

    // ------------------------------------------------------------
    // querying
    // ------------------------------------------------------------

    fn query() -> Builder<Self> {
        Builder::new()
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
        with_db(|ex| Self::find_on(ex, id))
    }

    fn find_on(ex: &dyn Executor, id: i64) -> OrmResult<Option<Self>> {
        Self::query().where_eq(Self::PRIMARY_KEY, id).first_on(ex)
    }

    /// Like `find`, but a missing row is an error that becomes a 404.
    fn find_or_fail(id: i64) -> OrmResult<Self> {
        Self::find(id)?.ok_or_else(|| OrmError::NotFound(format!("{} not found", Self::TABLE)))
    }

    /// Row -> model (applies `BOOLS`).
    fn from_row(mut row: Row) -> OrmResult<Self> {
        for col in Self::BOOLS {
            if let Some(n) = row.get(*col).and_then(Value::as_i64) {
                row.insert((*col).to_string(), Value::Bool(n != 0));
            }
        }
        serde_json::from_value(Value::Object(row))
            .map_err(|e| OrmError::Db(format!("row does not fit model for `{}`: {e}", Self::TABLE)))
    }

    // ------------------------------------------------------------
    // writing
    // ------------------------------------------------------------

    /// Keep only the `FILLABLE` keys of `attrs`.
    #[doc(hidden)]
    fn fillable_map<T: Serialize>(attrs: &T) -> OrmResult<Map<String, Value>> {
        let v = serde_json::to_value(attrs)
            .map_err(|e| OrmError::Invalid(format!("attributes are not serialisable: {e}")))?;
        let Value::Object(m) = v else {
            return Err(OrmError::Invalid("attributes must be an object".into()));
        };
        Ok(m.into_iter()
            .filter(|(k, _)| Self::FILLABLE.contains(&k.as_str()))
            .collect())
    }

    /// Insert a new row from the `FILLABLE` subset of `attrs` and return it.
    fn create<T: Serialize>(attrs: &T) -> OrmResult<Self> {
        with_db(|ex| Self::create_on(ex, attrs))
    }

    fn create_on<T: Serialize>(ex: &dyn Executor, attrs: &T) -> OrmResult<Self> {
        let map = Self::fillable_map(attrs)?;
        Self::insert_map(ex, map)
    }

    /// Insert **all** keys of `attrs`, ignoring `FILLABLE`. Only for data
    /// you built yourself — never for request input.
    fn force_create<T: Serialize>(attrs: &T) -> OrmResult<Self> {
        let v = serde_json::to_value(attrs)
            .map_err(|e| OrmError::Invalid(format!("attributes are not serialisable: {e}")))?;
        let Value::Object(map) = v else {
            return Err(OrmError::Invalid("attributes must be an object".into()));
        };
        with_db(|ex| Self::insert_map(ex, map))
    }

    #[doc(hidden)]
    fn insert_map(ex: &dyn Executor, mut map: Map<String, Value>) -> OrmResult<Self> {
        if Self::TIMESTAMPS {
            let now = now_iso();
            for k in ["created_at", "updated_at"] {
                if matches!(map.get(k), None | Some(Value::Null)) {
                    map.insert(k.to_string(), Value::String(now.clone()));
                }
            }
        }
        let explicit_id = map.get(Self::PRIMARY_KEY).and_then(Value::as_i64);
        let pairs: Vec<(String, SqlValue)> = map
            .iter()
            .filter(|(k, v)| !(k.as_str() == Self::PRIMARY_KEY && v.is_null()))
            .map(|(k, v)| (k.clone(), SqlValue::from_json(v)))
            .collect();

        let res = Query::new(Self::TABLE).insert_on(ex, pairs)?;
        let id = explicit_id.unwrap_or(res.last_insert_id);
        Self::find_any_on(ex, id)?
            .ok_or_else(|| OrmError::Db(format!("inserted `{}` row {id} vanished", Self::TABLE)))
    }

    #[doc(hidden)]
    fn find_any_on(ex: &dyn Executor, id: i64) -> OrmResult<Option<Self>> {
        Self::query()
            .with_trashed()
            .where_eq(Self::PRIMARY_KEY, id)
            .first_on(ex)
    }

    /// INSERT (no id yet) or UPDATE every column (id set). Writes the
    /// struct as it is — this is *your* data, so `FILLABLE` does not apply.
    fn save(&mut self) -> OrmResult<()> {
        with_db(|ex| self.save_on(ex))
    }

    fn save_on(&mut self, ex: &dyn Executor) -> OrmResult<()> {
        let mut map = match serde_json::to_value(&*self) {
            Ok(Value::Object(m)) => m,
            _ => {
                return Err(OrmError::Invalid(
                    "model must serialise to an object".into(),
                ));
            }
        };
        let fresh = match self.id() {
            None => Self::insert_map(ex, map)?,
            Some(id) => {
                map.remove(Self::PRIMARY_KEY);
                if Self::TIMESTAMPS {
                    map.insert("updated_at".into(), Value::String(now_iso()));
                }
                let pairs = pairs_from_json(&Value::Object(map))?;
                Query::new(Self::TABLE)
                    .where_eq(Self::PRIMARY_KEY, id)
                    .update_on(ex, pairs)?;
                Self::find_any_on(ex, id)?
                    .ok_or_else(|| OrmError::NotFound(format!("{} not found", Self::TABLE)))?
            }
        };
        *self = fresh;
        Ok(())
    }

    /// Update this row from the `FILLABLE` subset of `attrs`. Every key you
    /// pass is written (a `null` writes NULL), so pass only what changed.
    fn update<T: Serialize>(&mut self, attrs: &T) -> OrmResult<()> {
        with_db(|ex| self.update_on(ex, attrs))
    }

    fn update_on<T: Serialize>(&mut self, ex: &dyn Executor, attrs: &T) -> OrmResult<()> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot update an unsaved model".into()))?;
        let mut map = Self::fillable_map(attrs)?;
        if map.is_empty() {
            return Ok(());
        }
        if Self::TIMESTAMPS {
            map.insert("updated_at".into(), Value::String(now_iso()));
        }
        let pairs = pairs_from_json(&Value::Object(map))?;
        Query::new(Self::TABLE)
            .where_eq(Self::PRIMARY_KEY, id)
            .update_on(ex, pairs)?;
        *self = Self::find_any_on(ex, id)?
            .ok_or_else(|| OrmError::NotFound(format!("{} not found", Self::TABLE)))?;
        Ok(())
    }

    /// Delete this row (sets `deleted_at` when `SOFT_DELETES`).
    fn delete(&self) -> OrmResult<u64> {
        with_db(|ex| self.delete_on(ex))
    }

    fn delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot delete an unsaved model".into()))?;
        let q = Query::new(Self::TABLE).where_eq(Self::PRIMARY_KEY, id);
        if Self::SOFT_DELETES {
            q.update_on(ex, vec![("deleted_at".into(), SqlValue::Text(now_iso()))])
        } else {
            q.delete_on(ex)
        }
    }

    /// Really delete the row, even with `SOFT_DELETES`.
    fn force_delete(&self) -> OrmResult<u64> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot delete an unsaved model".into()))?;
        with_db(|ex| {
            Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .delete_on(ex)
        })
    }

    /// Undo a soft delete.
    fn restore(&mut self) -> OrmResult<()> {
        let id = self
            .id()
            .ok_or_else(|| OrmError::Invalid("cannot restore an unsaved model".into()))?;
        with_db(|ex| {
            Query::new(Self::TABLE)
                .where_eq(Self::PRIMARY_KEY, id)
                .update_on(ex, vec![("deleted_at".into(), SqlValue::Null)])?;
            *self = Self::find_any_on(ex, id)?
                .ok_or_else(|| OrmError::NotFound(format!("{} not found", Self::TABLE)))?;
            Ok(())
        })
    }

    /// Reload from the database.
    fn fresh(&self) -> OrmResult<Option<Self>> {
        match self.id() {
            Some(id) => with_db(|ex| Self::find_any_on(ex, id)),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------
    // output
    // ------------------------------------------------------------

    /// JSON for clients: the model without its `HIDDEN` columns.
    fn public_json(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(m) = &mut v {
            for h in Self::HIDDEN {
                m.remove(*h);
            }
        }
        v
    }

    /// `public_json` as a response.
    fn json_response(&self, status: Status) -> Response {
        Response::json(status, &self.public_json())
    }

    // ------------------------------------------------------------
    // relationships
    // ------------------------------------------------------------

    /// `post.has_many::<Comment>("post_id")` -> comments whose `post_id`
    /// is this model's id. Chain further conditions, then `get()`.
    fn has_many<R: Model>(&self, foreign_key: &str) -> Builder<R> {
        match self.id() {
            Some(id) => R::query().where_eq(foreign_key, id),
            None => R::query().where_raw("1 = 0", Vec::new()),
        }
    }

    /// Like `has_many`, but fetch the single related row.
    fn has_one<R: Model>(&self, foreign_key: &str) -> OrmResult<Option<R>> {
        self.has_many::<R>(foreign_key).first()
    }

    /// `comment.belongs_to::<Post>(comment.post_id)`.
    fn belongs_to<R: Model>(&self, foreign_key_value: Option<i64>) -> OrmResult<Option<R>> {
        match foreign_key_value {
            Some(id) => R::find(id),
            None => Ok(None),
        }
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
    q: Query,
    _m: PhantomData<fn() -> M>,
}

impl<M: Model> Default for Builder<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> Builder<M> {
    pub fn new() -> Self {
        let mut q = Query::new(M::TABLE);
        if M::SOFT_DELETES {
            q = q.soft_deletes("deleted_at");
        }
        Builder { q, _m: PhantomData }
    }

    fn map(self, f: impl FnOnce(Query) -> Query) -> Self {
        Builder {
            q: f(self.q),
            _m: PhantomData,
        }
    }

    pub fn where_(self, col: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        self.map(|q| q.where_(col, op, val))
    }
    pub fn where_eq(self, col: &str, val: impl Into<SqlValue>) -> Self {
        self.map(|q| q.where_eq(col, val))
    }
    pub fn or_where(self, col: &str, op: &str, val: impl Into<SqlValue>) -> Self {
        self.map(|q| q.or_where(col, op, val))
    }
    pub fn where_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.map(|q| q.where_in(col, vals))
    }
    pub fn where_not_in<I, V>(self, col: &str, vals: I) -> Self
    where
        I: IntoIterator<Item = V>,
        V: Into<SqlValue>,
    {
        self.map(|q| q.where_not_in(col, vals))
    }
    pub fn where_null(self, col: &str) -> Self {
        self.map(|q| q.where_null(col))
    }
    pub fn where_not_null(self, col: &str) -> Self {
        self.map(|q| q.where_not_null(col))
    }
    pub fn where_between(
        self,
        col: &str,
        lo: impl Into<SqlValue>,
        hi: impl Into<SqlValue>,
    ) -> Self {
        self.map(|q| q.where_between(col, lo, hi))
    }
    pub fn where_raw(self, sql: &'static str, params: Vec<SqlValue>) -> Self {
        self.map(|q| q.where_raw(sql, params))
    }
    pub fn where_group(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.map(|q| q.where_group(f))
    }
    pub fn or_where_group(self, f: impl FnOnce(Query) -> Query) -> Self {
        self.map(|q| q.or_where_group(f))
    }
    pub fn order_by(self, col: &str, direction: &str) -> Self {
        self.map(|q| q.order_by(col, direction))
    }
    pub fn order_by_desc(self, col: &str) -> Self {
        self.map(|q| q.order_by_desc(col))
    }
    pub fn latest(self) -> Self {
        self.map(|q| q.latest())
    }
    pub fn sort_by(self, requested: Option<&str>, allowed: &[&str], default: (&str, &str)) -> Self {
        self.map(|q| q.sort_by(requested, allowed, default))
    }
    pub fn limit(self, n: u64) -> Self {
        self.map(|q| q.limit(n))
    }
    pub fn offset(self, n: u64) -> Self {
        self.map(|q| q.offset(n))
    }
    pub fn with_trashed(self) -> Self {
        self.map(|q| q.with_trashed())
    }
    pub fn only_trashed(self) -> Self {
        self.map(|q| q.only_trashed())
    }
    pub fn all_rows(self) -> Self {
        self.map(|q| q.all_rows())
    }

    /// The generated SQL and bound values (debugging, tests).
    pub fn to_sql(&self) -> OrmResult<(String, Vec<SqlValue>)> {
        self.q.to_select_sql()
    }

    // ---------- running ----------

    pub fn get_on(&self, ex: &dyn Executor) -> OrmResult<Vec<M>> {
        self.q
            .get_on(ex)?
            .into_iter()
            .map(M::from_row)
            .collect::<OrmResult<Vec<M>>>()
    }

    pub fn first_on(&self, ex: &dyn Executor) -> OrmResult<Option<M>> {
        match self.q.first_on(ex)? {
            Some(row) => Ok(Some(M::from_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn count_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        self.q.count_on(ex)
    }

    pub fn exists_on(&self, ex: &dyn Executor) -> OrmResult<bool> {
        self.q.exists_on(ex)
    }

    /// `page` starts at 1. `per_page` is clamped to `1..=MAX_PER_PAGE`.
    pub fn paginate_on(&self, ex: &dyn Executor, page: u64, per_page: u64) -> OrmResult<Page<M>> {
        let per_page = per_page.clamp(1, MAX_PER_PAGE);
        let page = page.max(1);
        let total = self.q.count_on(ex)?;
        let rows = self
            .q
            .clone()
            .limit(per_page)
            .offset((page - 1).saturating_mul(per_page))
            .get_on(ex)?;
        let data = rows
            .into_iter()
            .map(M::from_row)
            .collect::<OrmResult<Vec<M>>>()?;
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

    /// Bulk-update the matching rows from the `FILLABLE` subset of `attrs`.
    /// Returns the number of rows changed.
    pub fn update_on<T: Serialize>(&self, ex: &dyn Executor, attrs: &T) -> OrmResult<u64> {
        let mut map = M::fillable_map(attrs)?;
        if map.is_empty() {
            return Ok(0);
        }
        if M::TIMESTAMPS {
            map.insert("updated_at".into(), Value::String(now_iso()));
        }
        self.q.update_on(ex, pairs_from_json(&Value::Object(map))?)
    }

    /// Bulk delete (soft when the model uses soft deletes).
    pub fn delete_on(&self, ex: &dyn Executor) -> OrmResult<u64> {
        if M::SOFT_DELETES {
            self.q
                .update_on(ex, vec![("deleted_at".into(), SqlValue::Text(now_iso()))])
        } else {
            self.q.delete_on(ex)
        }
    }

    pub fn get(&self) -> OrmResult<Vec<M>> {
        with_db(|ex| self.get_on(ex))
    }
    pub fn first(&self) -> OrmResult<Option<M>> {
        with_db(|ex| self.first_on(ex))
    }
    /// Like `first`, but a miss is a 404.
    pub fn first_or_fail(&self) -> OrmResult<M> {
        self.first()?
            .ok_or_else(|| OrmError::NotFound(format!("{} not found", M::TABLE)))
    }
    pub fn count(&self) -> OrmResult<u64> {
        with_db(|ex| self.count_on(ex))
    }
    pub fn exists(&self) -> OrmResult<bool> {
        with_db(|ex| self.exists_on(ex))
    }
    pub fn paginate(&self, page: u64, per_page: u64) -> OrmResult<Page<M>> {
        with_db(|ex| self.paginate_on(ex, page, per_page))
    }
    pub fn update<T: Serialize>(&self, attrs: &T) -> OrmResult<u64> {
        with_db(|ex| self.update_on(ex, attrs))
    }
    pub fn delete(&self) -> OrmResult<u64> {
        with_db(|ex| self.delete_on(ex))
    }
}

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
