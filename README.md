# fornthulani-plugin-orm

Eloquent-style models, relations with eager loading, query builder,
schema builder and migrations for
[fornthulani](https://github.com/thomas666-beast/fornthulani).
Drivers for **SQLite** (bundled, default) and **PostgreSQL** (feature
`postgres`, with a built-in connection pool). The same model code runs
on both.

```toml
[dependencies]
fornthulani-plugin-orm = { version = "0.2", features = ["postgres"] }
```

```rust
Db::set(PgConnection::connect(&std::env::var("DATABASE_URL")?)?);
// or: Db::set(SqliteConnection::open("app.db")?);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Post {
    id: Option<i64>,
    user_id: i64,
    title: String,
    published: bool,
    meta: serde_json::Value,
    created_at: Option<String>,
    updated_at: Option<String>,
    deleted_at: Option<String>,
    // relations and aggregates, filled when asked for
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author: Option<Box<User>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comments: Option<Vec<Comment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comments_count: Option<i64>,
}

impl Model for Post {
    const TABLE: &'static str = "posts";
    const SOFT_DELETES: bool = true;
    const FILLABLE: &'static [&'static str] = &["user_id", "title", "published", "meta"];
    const CASTS: &'static [(&'static str, Cast)] = &[("published", Cast::Bool), ("meta", Cast::Json)];
    fn id(&self) -> Option<i64> { self.id }

    fn relations() -> Vec<Relation> {
        vec![
            Relation::belongs_to::<User>("author", "user_id"),
            Relation::has_many::<Comment>("comments", "post_id"),
            Relation::belongs_to_many::<Tag>("tags", "post_tag", "post_id", "tag_id").with_pivot(&["role"]),
        ]
    }
}

let page = Post::with(&["author", "comments.author"])            // nested eager loading, no N+1
    .with_where("comments", |q| q.where_eq("approved", true))   // constrained eager load
    .with_count(&["comments"])                                  // comments_count
    .where_has("tags", |q| q.where_eq("name", "rust"))          // relationship existence
    .where_json("meta->lang", "=", "en")
    .sort_by(req.query_param("sort").as_deref(), &["title", "id"], ("id", "desc"))
    .paginate(1, 20)?;                                          // per_page capped at 100
Ok(page.json_response())                                       // HIDDEN stripped, recursively
```

## Security by construction

| Risk | What stops it |
|---|---|
| SQL injection via values | values are always bound `?` parameters |
| …via column/table names | identifiers validated (`[A-Za-z_][A-Za-z0-9_]*`, at most `a.b`, `t.*`) and quoted; JSON paths validated too |
| …via operators / sort direction | fixed whitelists |
| …via raw SQL | `where_raw`, `select_raw`, `having_raw`, `order_by_raw`, `Db::select/statement` take `&'static str`: only literals compile |
| client-chosen `?sort=` | `sort_by(requested, allowed, default)` whitelist, else 400 |
| mass assignment (`is_admin`) | `FILLABLE` allow-list, **empty by default** |
| leaking `password` | `HIDDEN` columns removed by `public_json()` / `Page::to_json()` |
| `DELETE FROM users` by accident | `update`/`delete` with no condition is refused (`all_rows()` to confirm) |
| `IN ()` matching everything | empty `IN` matches nothing |
| `?per_page=10000000` | `paginate` clamps to 1..=100 |
| leaking SQL/schema in errors | `OrmError` → `ThulaniError`: `NotFound`→404, constraint→409, bad query→400, DB failure→500 (logged, generic body) |
| forged pagination cursors | cursors are decoded and type-checked; garbage → 400 |
| `{"$ne": 1}` style payloads in validation | `unique`/`exists` reject objects and nulls, and fail closed on any DB error |
| leaking hidden fields of related models | `HIDDEN` is applied recursively to eager-loaded relations |
| upsert overwriting guarded columns | `upsert` / `insert_many` go through `FILLABLE` (Laravel's don't) |

## Models

Constants: `TABLE`, `PRIMARY_KEY` (`"id"`, integer), `TIMESTAMPS`
(`created_at`/`updated_at`), `SOFT_DELETES` (`deleted_at`), `FILLABLE`
(mass-assignment allow-list, empty by default), `HIDDEN`, `CASTS`
(`Bool`, `Int`, `Float`, `String`, `Json`), `BOOLS` (shorthand),
`CONNECTION` (a named connection), `PER_PAGE`.

| Task | Call |
|---|---|
| create | `create(&attrs)` (FILLABLE), `force_create`, `create_on(tx, ..)` |
| find | `find`, `find_or_fail` (→ 404), `find_many`, `first_where`, `all` |
| write | `save` (insert or update the struct), `update(&attrs)` (FILLABLE), `increment` / `decrement`, `touch` |
| find-or-* | `first_or_create(&search, &extra)`, `update_or_create(&search, &values)` |
| bulk | `insert_many(&rows)`, `upsert(&rows, &unique_by, &update)` (FILLABLE, chunked) |
| delete | `delete` (soft when `SOFT_DELETES`), `destroy(&ids)`, `force_delete`, `restore` |
| other | `fresh`, `replicate`, `load(&["rel"])`, `load_many`, `public_json`, `json_response` |

Lifecycle hooks (override any): `creating(&mut attrs, tx)` (can change
the row before INSERT, e.g. set a slug), `created`, `updating(&mut changes, tx)`,
`updated`, `deleting` (return `Err` to cancel), `deleted`.

Global scopes: `fn global_scope(q: Query) -> Query { q.where_eq("tenant_id", current_tenant()) }`,
applied to every query (including eager loads and `where_has` subqueries)
unless `without_global_scope()`.

## Relations

```rust
Relation::has_many::<Post>("posts", "user_id")
Relation::has_one::<Profile>("profile", "user_id")
Relation::belongs_to::<User>("author", "user_id")                // .owner_key("uuid")
Relation::belongs_to_many::<Tag>("tags", "post_tag", "post_id", "tag_id")
    .with_pivot(&["role"]).with_timestamps()                     // tag.pivot.role
Relation::has_many_through::<Post>("posts", "users", "country_id", "user_id")
```

- Eager loading: `with(&["posts.comments", "posts.tags"])`, `with_where(rel, |q| ..)`, `load` on an instance.
  One query per relation level (two for many-to-many), chunked for large `IN` lists.
- Existence: `has`, `doesnt_have`, `where_has`, `or_where_has`, `where_doesnt_have` (correlated `EXISTS`).
- Aggregates: `with_count`, `with_count_where`, `with_sum` / `with_avg` / `with_min` / `with_max`
  (`comments_sum_likes`), `with_exists` (`tags_exists`, a bool).
- Querying a relation: `post.related::<Comment>("comments").where_eq("approved", true).count()`.
- Pivots: `attach`, `attach_with(&ids, &json!({"role": "owner"}))`, `detach` (empty = all), `sync` (atomic, returns attached/detached).

## Query builder

Typed via `Model::query()` / `with(..)`, untyped (rows as JSON) via `Db::table("t")`.

- Where: `where_` `where_eq` `or_where` `where_in` `where_not_in` `where_null`
  `where_between` `where_not_between` `where_like` (case-insensitive: `ILIKE` on PostgreSQL,
  `LIKE` on SQLite) `where_column` `where_date` `where_json("meta->a->b", op, v)`
  `where_in_sub` `where_exists` `where_group` / `or_where_group` `where_all(&json)` `where_raw`.
- Select: `select`, `select_raw`, `distinct`, `join`, `left_join`, `alias`, `group_by`, `having_raw`.
- Order / limit: `order_by`, `order_by_desc`, `latest`, `oldest`, `reorder`, `order_by_raw`,
  `sort_by` (whitelisted client sort), `limit`/`take`, `offset`/`skip`.
- Locks (PostgreSQL): `lock_for_update`, `shared_lock`; no-ops on SQLite (it locks the whole db).
- Conditional: `when(cond, |q| ..)`, `scope(fn)`.
- Results: `get` `first` `first_or_fail` `sole` `count` `exists` `doesnt_exist`
  `sum` `avg` `min` `max` `pluck` `value` `chunk_by_id`.
- Pagination: `paginate` (with total), `simple_paginate` (no COUNT),
  `cursor_paginate` / `cursor_paginate_desc` (keyset on the primary key, stable under inserts).
- Writes: `update`, `increment`, `decrement`, `delete`, `force_delete`,
  `insert`, `insert_get_id`, `insert_many`, `insert_or_ignore`, `upsert`.

`update(&attrs)` writes every key you pass (`null` writes NULL). With a
typed input struct use `#[serde(skip_serializing_if = "Option::is_none")]`.

## PostgreSQL

```rust
Db::set(
    PgConnection::builder("postgres://app:secret@db/app?sslmode=require")
        .max_connections(20)                     // default 10
        .checkout_timeout(Duration::from_secs(5))
        .statement_timeout_ms(5_000)             // kill runaway queries
        .connect()?,                             // fails fast on bad credentials
);
Db::add_connection("analytics", PgConnection::connect(&analytics_url)?); // + const CONNECTION
```

- Built-in thread-safe pool; dead connections are replaced, a failed
  `ROLLBACK` discards the connection instead of returning it dirty.
- `?` placeholders become `$1..$n` (never inside strings, quoted identifiers, comments or `$$` bodies).
- Values are bound in text format and typed by the server, so form strings
  like `"42"` compare against `BIGINT`, ISO strings go into `TIMESTAMPTZ`, JSON into `JSONB`.
- Results decode to natural JSON: integers, `NUMERIC` (number when exact,
  else a string, so money never silently loses precision), `TIMESTAMPTZ`
  (`2026-03-01T12:30:00.25Z`), `DATE`, `UUID`, `JSONB`, arrays, `BYTEA` (`\x..`), `INET`, enums.
- Errors: SQLSTATE class 23 (unique, FK, not-null, check) → 409, class 22 (bad input) → 400, other → 500.
- Schema builder emits native types: `BIGINT GENERATED BY DEFAULT AS IDENTITY`,
  `BOOLEAN`, `TIMESTAMPTZ`, `JSONB`, `UUID`, `NUMERIC(p,s)`, `BYTEA`, `DOUBLE PRECISION`.
- TLS with feature `postgres-tls`: `sslmode=prefer` (default) encrypts when the
  server offers it, like libpq; `sslmode=require` also verifies the certificate
  and host name (libpq's `verify-full`). Without the feature, `require` is an error.

## Eloquent comparison

| | Eloquent | this ORM |
|---|---|---|
| models, timestamps, soft deletes, casts, hidden | yes | yes, as typed Rust structs |
| has_one / has_many / belongs_to / belongs_to_many / has_many_through | yes | yes |
| nested + constrained eager loading, `withCount`/`withSum`/`withExists` | yes | yes |
| `whereHas` / `doesntHave`, pivots with `attach`/`sync`/`detach` | yes | yes |
| `firstOrCreate`, `updateOrCreate`, `upsert`, `increment`, chunking | yes | yes |
| `paginate` / `simplePaginate` / `cursorPaginate` | yes | yes |
| global scopes, model events, `DB::listen`, named connections | yes | yes |
| raw SQL only from literals | no (`whereRaw($input)` compiles) | **yes**: `&'static str`, checked at compile time |
| mass assignment | opt-in `$fillable` / `$guarded` | **empty allow-list by default**, also enforced by `upsert` / `insert_many` |
| update/delete without `WHERE` | runs | **refused** unless `all_rows()` |
| client `?per_page` / `?sort` | up to you | **capped / whitelisted** |
| error leaks | depends on debug mode | **always** mapped to safe HTTP status codes |
| row ↔ model mapping | magic properties (typos read as `null`) | **typed structs**: a missing or mistyped column is an error, not a silent `null` |
| polymorphic relations, accessors/mutators, factories, MySQL/SQL Server, async | yes | not yet (see Limits) |

## Transactions

```rust
Db::transaction(|tx| {
    let order = Order::query().where_eq("id", id).lock_for_update().first_on(tx)?.ok_or(..)?;
    Item::create_on(tx, &item_for(&order))?;
    Ok(order)
})?;
Db::transaction_on("analytics", |tx| ..)?;
```
Use the `tx` you are given (`*_on(tx)`); the global methods would take a
different connection. Commit on `Ok`, rollback on `Err` or panic.

## Query log

```rust
fn log(e: &QueryEvent<'_>) { eprintln!("{:?} {} ({:?})", e.dialect, e.sql, e.elapsed) }
Db::listen(log);
```
`e.params` holds the bound values: treat them as sensitive (passwords, tokens).

## Migrations

```rust
pub fn up(db: &dyn Executor) -> OrmResult<()> {
    Schema::create(db, "posts", |t| {
        t.id();
        t.foreign_id("user_id").constrained("users").cascade_on_delete();
        t.string("title", 200).unique();
        t.boolean("published").default(false);
        t.json("meta").nullable();
        t.decimal("price", 10, 2).default(0);
        t.timestamps();
        t.soft_deletes();
        t.index(&["user_id", "published"]);
    })
}
Schema::table(db, "posts", |t| { t.string("subtitle", 100).nullable(); t.drop_index(&["user_id"]); })?;
Schema::has_table(db, "posts")?; Schema::has_column(db, "posts", "slug")?;

let conn = Db::get()?;
Migrator::new(vec![Migration { name: "20260101_create_posts", up, down }]).run(&*conn)?;
```
Tracked in a `_migrations` table, one transaction per migration (DDL is
transactional on both drivers), `rollback()` undoes the last batch.

## Validation integration

Enable feature `validation` and call `register_validation_rules()` once.
The rules run through the ORM, so they work on SQLite and PostgreSQL alike.

```rust
req.validate([
    ("email",    "required|email|unique:users"),                 // column = attribute
    ("team_id",  "required|integer|exists:teams,id"),
    ("tags",     "array|exists:tags,id"),                        // every element must exist
    ("plan",     "exists:billing.plans,code,active,true"),       // PG schema + extra where
])?;
// editing: ignore the current row, and soft-deleted rows
("email", format!("required|email|unique:users,email,{id},id,deleted_at,NULL"))
```

`unique:[connection.]table[,column[,ignore[,id_column[,col,val]...]]]` and
`exists:[connection.]table[,column[,col,val]...]`. Where values: `NULL`,
`NOT_NULL`, `!value` (not equal), `true`/`false`, integers. A prefix is a
named connection if one is registered under that name, otherwise a
PostgreSQL schema. Both rules fail closed.

## Testing

```sh
cargo test                       # SQLite
cargo test --all-features        # + validation; PostgreSQL tests skip without a database
ORM_PG_URL=postgres://postgres:postgres@localhost/orm_test cargo test --all-features
```
The same end-to-end scenario (`tests/common`) runs on both drivers.

## Limits (v0.2)

- Integer primary keys (`i64`). No composite keys on models.
- No polymorphic (`morphTo`) relations, accessors/mutators or factories yet.
- No MySQL / SQL Server driver (implement the small `Connection` trait).
- Synchronous: run queries from fornthulani's worker threads, not an async runtime.
- In raw SQL every `?` is a placeholder; for PostgreSQL's jsonb `?` operator
  use `jsonb_exists(col, key)` instead.
- Bulk operations (`insert_many`, `upsert`, query `update`/`delete`) do not fire model hooks, as in Laravel.
- SQLite: one connection behind a mutex; `lock_for_update` is a no-op.
