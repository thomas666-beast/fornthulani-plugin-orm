//! Relationships: definitions, eager loading (no N+1), existence queries
//! and pivot-table helpers.
//!
//! ```ignore
//! impl Model for Post {
//!     // ...
//!     fn relations() -> Vec<Relation> {
//!         vec![
//!             Relation::belongs_to::<User>("author", "user_id"),
//!             Relation::has_many::<Comment>("comments", "post_id"),
//!             Relation::belongs_to_many::<Tag>("tags", "post_tag", "post_id", "tag_id").with_pivot(&["role"]),
//!         ]
//!     }
//! }
//!
//! // struct Post { ..., #[serde(default, skip_serializing_if = "Option::is_none")] comments: Option<Vec<Comment>> }
//! let posts = Post::query().with(&["author", "comments.author", "tags"]).get()?;   // 4 queries total
//! ```
//!
//! Eager loading works on rows before they are deserialised: the related
//! rows are attached under the relation name, so a struct field with that
//! name (usually `Option<Vec<R>>` / `Option<R>` with `#[serde(default)]`)
//! receives them. Relation keys and `<name>_count` keys are never written
//! back by `save()`.

use crate::conn::Executor;
use crate::error::{OrmError, OrmResult};
use crate::model::Model;
use crate::query::{Agg, Joiner, Query, SubSelect};
use crate::value::{Row, SqlValue};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// A constraint applied to an eager load or an existence query.
pub type Constraint = Arc<dyn Fn(Query) -> Query + Send + Sync>;

/// Batch size for `WHERE key IN (...)` during eager loading.
const EAGER_CHUNK: usize = 1000;

/// What we need to know about the related model, captured as plain data and
/// function pointers so relations can be nested and recursive.
#[derive(Clone, Copy)]
pub(crate) struct RelatedMeta {
    pub table: &'static str,
    pub pk: &'static str,
    pub soft: bool,
    pub hidden: &'static [&'static str],
    pub cast: fn(&mut Row),
    pub relations: fn() -> Vec<Relation>,
    pub scope: fn(Query) -> Query,
}

impl RelatedMeta {
    fn of<R: Model>() -> Self {
        RelatedMeta {
            table: R::TABLE,
            pk: R::PRIMARY_KEY,
            soft: R::SOFT_DELETES,
            hidden: R::HIDDEN,
            cast: R::cast_row,
            relations: R::relations,
            scope: R::global_scope,
        }
    }

    /// A query on the related table: soft-delete filter, the user's
    /// constraint, then the related model's global scope, each grouped.
    fn base(&self, constraint: Option<&Constraint>) -> Query {
        let mut q = Query::new(self.table);
        if self.soft {
            q = q.soft_deletes("deleted_at");
        }
        if let Some(c) = constraint {
            q = c(q).grouped();
        }
        q.with_scope(self.scope)
    }
}

#[derive(Clone, Debug)]
pub(crate) enum Kind {
    HasMany {
        foreign_key: &'static str,
        local_key: &'static str,
    },
    HasOne {
        foreign_key: &'static str,
        local_key: &'static str,
    },
    BelongsTo {
        foreign_key: &'static str,
        owner_key: &'static str,
    },
    BelongsToMany {
        pivot: &'static str,
        foreign_pivot_key: &'static str,
        related_pivot_key: &'static str,
        parent_key: &'static str,
        related_key: &'static str,
        pivot_columns: &'static [&'static str],
        pivot_timestamps: bool,
    },
    HasManyThrough {
        through: &'static str,
        first_key: &'static str,
        second_key: &'static str,
        local_key: &'static str,
        through_key: &'static str,
    },
}

/// One relationship of a model. Build with the constructors and return the
/// list from `Model::relations()`.
#[derive(Clone)]
pub struct Relation {
    pub(crate) name: &'static str,
    pub(crate) kind: Kind,
    pub(crate) meta: RelatedMeta,
}

impl std::fmt::Debug for Relation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Relation")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("related", &self.meta.table)
            .finish()
    }
}

impl Relation {
    /// `posts.user_id -> users.id`, seen from `User`: `has_many::<Post>("posts", "user_id")`.
    pub fn has_many<R: Model>(name: &'static str, foreign_key: &'static str) -> Self {
        Relation {
            name,
            kind: Kind::HasMany {
                foreign_key,
                local_key: "id",
            },
            meta: RelatedMeta::of::<R>(),
        }
    }

    pub fn has_one<R: Model>(name: &'static str, foreign_key: &'static str) -> Self {
        Relation {
            name,
            kind: Kind::HasOne {
                foreign_key,
                local_key: "id",
            },
            meta: RelatedMeta::of::<R>(),
        }
    }

    /// Seen from `Post`: `belongs_to::<User>("author", "user_id")`.
    pub fn belongs_to<R: Model>(name: &'static str, foreign_key: &'static str) -> Self {
        Relation {
            name,
            kind: Kind::BelongsTo {
                foreign_key,
                owner_key: R::PRIMARY_KEY,
            },
            meta: RelatedMeta::of::<R>(),
        }
    }

    /// Many-to-many through a pivot table:
    /// `belongs_to_many::<Tag>("tags", "post_tag", "post_id", "tag_id")`.
    pub fn belongs_to_many<R: Model>(
        name: &'static str,
        pivot: &'static str,
        foreign_pivot_key: &'static str,
        related_pivot_key: &'static str,
    ) -> Self {
        Relation {
            name,
            kind: Kind::BelongsToMany {
                pivot,
                foreign_pivot_key,
                related_pivot_key,
                parent_key: "id",
                related_key: R::PRIMARY_KEY,
                pivot_columns: &[],
                pivot_timestamps: false,
            },
            meta: RelatedMeta::of::<R>(),
        }
    }

    /// `Country -> users(country_id) -> posts(user_id)`, seen from `Country`:
    /// `has_many_through::<Post>("posts", "users", "country_id", "user_id")`.
    pub fn has_many_through<R: Model>(
        name: &'static str,
        through: &'static str,
        first_key: &'static str,
        second_key: &'static str,
    ) -> Self {
        Relation {
            name,
            kind: Kind::HasManyThrough {
                through,
                first_key,
                second_key,
                local_key: "id",
                through_key: "id",
            },
            meta: RelatedMeta::of::<R>(),
        }
    }

    /// The parent's key column when it is not `id` (has_many / has_one /
    /// has_many_through / belongs_to_many).
    pub fn local_key(mut self, key: &'static str) -> Self {
        match &mut self.kind {
            Kind::HasMany { local_key, .. }
            | Kind::HasOne { local_key, .. }
            | Kind::HasManyThrough { local_key, .. } => *local_key = key,
            Kind::BelongsToMany { parent_key, .. } => *parent_key = key,
            Kind::BelongsTo { .. } => {}
        }
        self
    }

    /// The related model's key column a `belongs_to` points at.
    pub fn owner_key(mut self, key: &'static str) -> Self {
        if let Kind::BelongsTo { owner_key, .. } = &mut self.kind {
            *owner_key = key;
        }
        self
    }

    /// Extra pivot columns to return under `"pivot"` on each related row.
    pub fn with_pivot(mut self, cols: &'static [&'static str]) -> Self {
        if let Kind::BelongsToMany { pivot_columns, .. } = &mut self.kind {
            *pivot_columns = cols;
        }
        self
    }

    /// Fill `created_at` / `updated_at` on the pivot table in `attach`.
    pub fn with_timestamps(mut self) -> Self {
        if let Kind::BelongsToMany {
            pivot_timestamps, ..
        } = &mut self.kind
        {
            *pivot_timestamps = true;
        }
        self
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn related_table(&self) -> &'static str {
        self.meta.table
    }

    /// The parent column whose value identifies the related rows.
    pub(crate) fn parent_key(&self) -> &'static str {
        match &self.kind {
            Kind::HasMany { local_key, .. }
            | Kind::HasOne { local_key, .. }
            | Kind::HasManyThrough { local_key, .. } => local_key,
            Kind::BelongsTo { foreign_key, .. } => foreign_key,
            Kind::BelongsToMany { parent_key, .. } => parent_key,
        }
    }

    /// Correlated sub-query matching related rows of the outer `parent`
    /// table (for `where_has`, `with_count`, ...).
    pub(crate) fn existence_query(&self, parent: &str, constraint: Option<&Constraint>) -> Query {
        let rt = self.meta.table;
        // self-referencing relation: give the inner table an alias
        let mut q = self.meta.base(constraint);
        let inner = if rt == parent {
            q = q.alias("_rel");
            "_rel".to_string()
        } else {
            rt.to_string()
        };
        match &self.kind {
            Kind::HasMany {
                foreign_key,
                local_key,
            }
            | Kind::HasOne {
                foreign_key,
                local_key,
            } => q.where_column(
                &format!("{inner}.{foreign_key}"),
                "=",
                &format!("{parent}.{local_key}"),
            ),
            Kind::BelongsTo {
                foreign_key,
                owner_key,
            } => q.where_column(
                &format!("{inner}.{owner_key}"),
                "=",
                &format!("{parent}.{foreign_key}"),
            ),
            Kind::BelongsToMany {
                pivot,
                foreign_pivot_key,
                related_pivot_key,
                parent_key,
                related_key,
                ..
            } => q
                .join(
                    pivot,
                    &format!("{pivot}.{related_pivot_key}"),
                    "=",
                    &format!("{inner}.{related_key}"),
                )
                .where_column(
                    &format!("{pivot}.{foreign_pivot_key}"),
                    "=",
                    &format!("{parent}.{parent_key}"),
                ),
            Kind::HasManyThrough {
                through,
                first_key,
                second_key,
                local_key,
                through_key,
            } => q
                .join(
                    through,
                    &format!("{through}.{through_key}"),
                    "=",
                    &format!("{inner}.{second_key}"),
                )
                .where_column(
                    &format!("{through}.{first_key}"),
                    "=",
                    &format!("{parent}.{local_key}"),
                ),
        }
    }
}

pub(crate) fn find_relation(rels: &[Relation], name: &str, table: &str) -> OrmResult<Relation> {
    rels.iter()
        .find(|r| r.name == name)
        .cloned()
        .ok_or_else(|| OrmError::Invalid(format!("`{table}` has no relation `{name}`")))
}

/// `where_has` / `doesnt_have` on a parent query.
pub(crate) fn add_has(
    q: Query,
    rels: &[Relation],
    parent: &str,
    name: &str,
    constraint: Option<Constraint>,
    negate: bool,
    joiner: Joiner,
) -> Query {
    match find_relation(rels, name, parent) {
        Ok(rel) => q.add_exists(
            joiner,
            rel.existence_query(parent, constraint.as_ref()),
            negate,
        ),
        Err(e) => {
            let mut q = q;
            q.fail(e);
            q
        }
    }
}

/// `with_count` / `with_sum` / ... on a parent query.
pub(crate) fn add_aggregate(
    q: Query,
    rels: &[Relation],
    parent: &str,
    name: &str,
    agg: Agg,
    col: Option<&str>,
    constraint: Option<Constraint>,
) -> Query {
    match find_relation(rels, name, parent) {
        Ok(rel) => {
            let alias = match col {
                None => format!("{name}_count"),
                Some(c) => format!("{name}_{}_{c}", agg.name()),
            };
            let sub = rel.existence_query(parent, constraint.as_ref());
            let col = col.map(|c| {
                format!(
                    "{}.{c}",
                    if rel.meta.table == parent {
                        "_rel"
                    } else {
                        rel.meta.table
                    }
                )
            });
            q.select_sub(sub, SubSelect::Agg(agg, col), alias)
        }
        Err(e) => {
            let mut q = q;
            q.fail(e);
            q
        }
    }
}

/// `with_exists`: adds `EXISTS (...) AS "<name>_exists"`.
pub(crate) fn add_exists_select(q: Query, rels: &[Relation], parent: &str, name: &str) -> Query {
    match find_relation(rels, name, parent) {
        Ok(rel) => q.select_sub(
            rel.existence_query(parent, None),
            SubSelect::Exists,
            format!("{name}_exists"),
        ),
        Err(e) => {
            let mut q = q;
            q.fail(e);
            q
        }
    }
}

// ----------------------------------------------------------------------
// eager loading
// ----------------------------------------------------------------------

/// A requested eager load: `"comments.author"` plus an optional constraint
/// for its last segment.
#[derive(Clone)]
pub(crate) struct With {
    pub path: String,
    pub constraint: Option<Constraint>,
}

/// Value -> comparable key (`5` and `"5"` match, as they do in SQL).
fn key_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(match n.as_i64() {
            Some(i) => i.to_string(),
            None => n.to_string(),
        }),
        Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        other => Some(other.to_string()),
    }
}

fn collect_keys(rows: &[Row], col: &str) -> Vec<SqlValue> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        if let Some(v) = r.get(col)
            && let Some(k) = key_of(v)
            && seen.insert(k)
        {
            out.push(SqlValue::from_json(v));
        }
    }
    out
}

/// `SELECT related WHERE col IN (keys)`, in chunks.
fn fetch_in(
    ex: &dyn Executor,
    base: &Query,
    col: &str,
    keys: &[SqlValue],
    pk: &str,
) -> OrmResult<Vec<Row>> {
    let mut out = Vec::new();
    let base = if base.has_orders() {
        base.clone()
    } else {
        base.clone().order_by(pk, "asc")
    };
    for chunk in keys.chunks(EAGER_CHUNK) {
        out.extend(
            base.clone()
                .where_in(col, chunk.iter().cloned())
                .get_on(ex)?,
        );
    }
    Ok(out)
}

pub(crate) fn eager_load(
    ex: &dyn Executor,
    rows: &mut [Row],
    rels: &[Relation],
    table: &str,
    with: &[With],
) -> OrmResult<()> {
    if rows.is_empty() || with.is_empty() {
        return Ok(());
    }
    // group "a", "a.b", "a.c" under "a"
    let mut groups: Vec<(String, Option<Constraint>, Vec<With>)> = Vec::new();
    for w in with {
        let (first, rest) = match w.path.split_once('.') {
            Some((f, r)) => (f.to_string(), Some(r.to_string())),
            None => (w.path.clone(), None),
        };
        let idx = match groups.iter().position(|g| g.0 == first) {
            Some(i) => i,
            None => {
                groups.push((first, None, Vec::new()));
                groups.len() - 1
            }
        };
        match rest {
            None => {
                if w.constraint.is_some() {
                    groups[idx].1 = w.constraint.clone();
                }
            }
            Some(r) => groups[idx].2.push(With {
                path: r,
                constraint: w.constraint.clone(),
            }),
        }
    }
    for (name, constraint, nested) in groups {
        let rel = find_relation(rels, &name, table)?;
        load_relation(ex, rows, &rel, constraint.as_ref(), &nested)?;
    }
    Ok(())
}

fn finish_related(
    ex: &dyn Executor,
    rel: &Relation,
    related: &mut [Row],
    nested: &[With],
) -> OrmResult<()> {
    eager_load(ex, related, &(rel.meta.relations)(), rel.meta.table, nested)?;
    for r in related.iter_mut() {
        (rel.meta.cast)(r);
    }
    Ok(())
}

fn load_relation(
    ex: &dyn Executor,
    rows: &mut [Row],
    rel: &Relation,
    constraint: Option<&Constraint>,
    nested: &[With],
) -> OrmResult<()> {
    let meta = rel.meta;
    let base = meta.base(constraint);
    match &rel.kind {
        Kind::HasMany {
            foreign_key,
            local_key,
        }
        | Kind::HasOne {
            foreign_key,
            local_key,
        } => {
            let keys = collect_keys(rows, local_key);
            let mut related = fetch_in(ex, &base, foreign_key, &keys, meta.pk)?;
            finish_related(ex, rel, &mut related, nested)?;
            let mut by: HashMap<String, Vec<Value>> = HashMap::new();
            for r in related {
                if let Some(k) = r.get(*foreign_key).and_then(key_of) {
                    by.entry(k).or_default().push(Value::Object(r));
                }
            }
            let one = matches!(rel.kind, Kind::HasOne { .. });
            for row in rows.iter_mut() {
                let list = row
                    .get(*local_key)
                    .and_then(key_of)
                    .and_then(|k| by.get(&k))
                    .cloned()
                    .unwrap_or_default();
                let v = if one {
                    list.into_iter().next().unwrap_or(Value::Null)
                } else {
                    Value::Array(list)
                };
                row.insert(rel.name.to_string(), v);
            }
        }
        Kind::BelongsTo {
            foreign_key,
            owner_key,
        } => {
            let keys = collect_keys(rows, foreign_key);
            let mut related = fetch_in(ex, &base, owner_key, &keys, meta.pk)?;
            finish_related(ex, rel, &mut related, nested)?;
            let by: HashMap<String, Value> = related
                .into_iter()
                .filter_map(|r| Some((r.get(*owner_key).and_then(key_of)?, Value::Object(r))))
                .collect();
            for row in rows.iter_mut() {
                let v = row
                    .get(*foreign_key)
                    .and_then(key_of)
                    .and_then(|k| by.get(&k))
                    .cloned()
                    .unwrap_or(Value::Null);
                row.insert(rel.name.to_string(), v);
            }
        }
        Kind::BelongsToMany {
            pivot,
            foreign_pivot_key,
            related_pivot_key,
            parent_key,
            related_key,
            pivot_columns,
            ..
        } => {
            let keys = collect_keys(rows, parent_key);
            let mut cols: Vec<&str> = vec![foreign_pivot_key, related_pivot_key];
            cols.extend(pivot_columns.iter().copied());
            let pq = Query::new(pivot).select(cols);
            let mut pivots = Vec::new();
            for chunk in keys.chunks(EAGER_CHUNK) {
                pivots.extend(
                    pq.clone()
                        .where_in(foreign_pivot_key, chunk.iter().cloned())
                        .get_on(ex)?,
                );
            }
            let rkeys = collect_keys(&pivots, related_pivot_key);
            let mut related = fetch_in(ex, &base, related_key, &rkeys, meta.pk)?;
            finish_related(ex, rel, &mut related, nested)?;
            let by_id: HashMap<String, Row> = related
                .into_iter()
                .filter_map(|r| Some((r.get(*related_key).and_then(key_of)?, r)))
                .collect();
            let mut by_parent: HashMap<String, Vec<Value>> = HashMap::new();
            for p in pivots {
                let (Some(pk), Some(rk)) = (
                    p.get(*foreign_pivot_key).and_then(key_of),
                    p.get(*related_pivot_key).and_then(key_of),
                ) else {
                    continue;
                };
                if let Some(r) = by_id.get(&rk) {
                    let mut r = r.clone();
                    r.insert("pivot".into(), Value::Object(p));
                    by_parent.entry(pk).or_default().push(Value::Object(r));
                }
            }
            for row in rows.iter_mut() {
                let list = row
                    .get(*parent_key)
                    .and_then(key_of)
                    .and_then(|k| by_parent.get(&k))
                    .cloned()
                    .unwrap_or_default();
                row.insert(rel.name.to_string(), Value::Array(list));
            }
        }
        Kind::HasManyThrough {
            through,
            first_key,
            second_key,
            local_key,
            through_key,
        } => {
            let keys = collect_keys(rows, local_key);
            let tq = Query::new(through).select([*through_key, *first_key]);
            let mut mids = Vec::new();
            for chunk in keys.chunks(EAGER_CHUNK) {
                mids.extend(
                    tq.clone()
                        .where_in(first_key, chunk.iter().cloned())
                        .get_on(ex)?,
                );
            }
            let mid_to_parent: HashMap<String, String> = mids
                .iter()
                .filter_map(|m| {
                    Some((
                        m.get(*through_key).and_then(key_of)?,
                        m.get(*first_key).and_then(key_of)?,
                    ))
                })
                .collect();
            let tkeys = collect_keys(&mids, through_key);
            let mut related = fetch_in(ex, &base, second_key, &tkeys, meta.pk)?;
            finish_related(ex, rel, &mut related, nested)?;
            let mut by_parent: HashMap<String, Vec<Value>> = HashMap::new();
            for r in related {
                if let Some(parent) = r
                    .get(*second_key)
                    .and_then(key_of)
                    .and_then(|k| mid_to_parent.get(&k))
                {
                    by_parent
                        .entry(parent.clone())
                        .or_default()
                        .push(Value::Object(r));
                }
            }
            for row in rows.iter_mut() {
                let list = row
                    .get(*local_key)
                    .and_then(key_of)
                    .and_then(|k| by_parent.get(&k))
                    .cloned()
                    .unwrap_or_default();
                row.insert(rel.name.to_string(), Value::Array(list));
            }
        }
    }
    Ok(())
}

/// Remove `HIDDEN` columns, recursively through loaded relations.
pub(crate) fn strip_hidden(v: &mut Value, hidden: &[&str], rels: &[Relation]) {
    match v {
        Value::Object(m) => {
            for h in hidden {
                m.remove(*h);
            }
            for r in rels {
                if let Some(child) = m.get_mut(r.name) {
                    let child_rels = (r.meta.relations)();
                    strip_hidden(child, r.meta.hidden, &child_rels);
                }
            }
        }
        Value::Array(a) => {
            for x in a {
                strip_hidden(x, hidden, rels);
            }
        }
        _ => {}
    }
}

/// Keys that are relation data, not columns (never written by `save`).
pub(crate) fn non_column_keys(rels: &[Relation], m: &mut Map<String, Value>) {
    for r in rels {
        m.remove(r.name);
        m.remove(&format!("{}_count", r.name));
        let aggs: Vec<String> = ["sum", "avg", "min", "max"]
            .iter()
            .map(|a| format!("{}_{a}_", r.name))
            .collect();
        m.retain(|k, _| !aggs.iter().any(|p| k.starts_with(p.as_str())));
    }
    m.remove("pivot");
}

/// Result of `sync`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncResult {
    pub attached: Vec<i64>,
    pub detached: Vec<i64>,
}
