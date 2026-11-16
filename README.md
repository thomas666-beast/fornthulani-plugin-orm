# fornthulani-plugin-orm

Eloquent-style models, query builder and migrations for
[fornthulani](https://github.com/thomas666-beast/fornthulani). SQLite driver included (bundled); other
databases implement the small `Connection` trait.

```rust
Db::set(SqliteConnection::open("app.db")?);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Post {
    id: Option<i64>,
    title: String,
    published: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
}

impl Model for Post {
    const TABLE: &'static str = "posts";
    const FILLABLE: &'static [&'static str] = &["title", "published"];
    const BOOLS: &'static [&'static str] = &["published"];
    fn id(&self) -> Option<i64> { self.id }
}

let post = Post::create(&json!({"title": "Hello"}))?;          // mass-assignment safe
let post = Post::find_or_fail(1)?;                              // missing -> 404 via `?`
let page = Post::query()
    .where_eq("published", true)
    .or_where_group(|g| g.where_eq("title", "x").or_where("title", "=", "y"))
    .sort_by(req.query_param("sort").as_deref(), &["title", "id"], ("id", "desc"))
    .paginate(1, 20)?;                                          // per_page capped at 100
Ok(page.json_response())
```

## Security by construction

| Risk | What stops it |
|---|---|
| SQL injection via values | values are always bound `?` parameters |
| …via column/table names | identifiers validated (`[A-Za-z_][A-Za-z0-9_]*`, at most `a.b`) and quoted |
| …via operators / sort direction | fixed whitelists |
| …via raw SQL | `where_raw` and `Db::select/statement` take `&'static str`: only literals compile |
| client-chosen `?sort=` | `sort_by(requested, allowed, default)` whitelist, else 400 |
| mass assignment (`is_admin`) | `FILLABLE` allow-list, **empty by default** |
| leaking `password` | `HIDDEN` columns removed by `public_json()` / `Page::to_json()` |
| `DELETE FROM users` by accident | `update`/`delete` with no condition is refused (`all_rows()` to confirm) |
| `IN ()` matching everything | empty `IN` matches nothing |
| `?per_page=10000000` | `paginate` clamps to 1..=100 |
| leaking SQL/schema in errors | `OrmError` → `ThulaniError`: `NotFound`→404, constraint→409, bad query→400, DB failure→500 (logged, generic body) |

## Model reference

`create` / `create_on(tx)`, `force_create` (unguarded: your own data only),
`find`, `find_or_fail`, `all`, `query()`, `save` (writes the struct),
`update(&attrs)` (FILLABLE subset), `delete`, `force_delete`, `restore`,
`fresh`, `public_json`, `json_response(status)`, `has_many::<R>("fk")`,
`has_one`, `belongs_to`.

Constants: `TABLE`, `PRIMARY_KEY` (`"id"`, integer), `TIMESTAMPS`
(`created_at`/`updated_at`, ISO-8601 UTC), `SOFT_DELETES` (`deleted_at`;
`with_trashed()`, `only_trashed()`), `FILLABLE`, `HIDDEN`, `BOOLS`.

Builder: `where_` `where_eq` `or_where` `where_in` `where_not_in`
`where_null` `where_not_null` `where_between` `where_group`
`or_where_group` `where_raw` `order_by` `latest` `sort_by` `limit`
`offset` → `get` `first` `first_or_fail` `count` `exists` `paginate`
`update` `delete`. `Db::table("t")` gives the same untyped (rows as JSON).

`update(&attrs)` writes every key you pass (`null` writes NULL). With a
typed input struct use `#[serde(skip_serializing_if = "Option::is_none")]`
(the generator does).

## Transactions

```rust
Db::transaction(|tx| {
    let order = Order::create_on(tx, &new_order)?;
    Item::create_on(tx, &item_for(&order))?;
    Ok(order)
})?;
```
Use the `tx` you are given (`*_on(tx)`); the global methods would try to
take the connection again. Commit on `Ok`, rollback on `Err` or panic.

## Migrations

```rust
pub fn up(db: &dyn Executor) -> OrmResult<()> {
    Schema::create(db, "posts", |t| {
        t.id();
        t.string("title", 200).unique();
        t.boolean("published").default(false);
        t.integer("user_id").references("users", "id").on_delete_cascade();
        t.timestamps();
        t.soft_deletes();
        t.index(&["user_id"]);
    })
}
let conn = Db::get()?;
Migrator::new(vec![Migration { name: "20260101_create_posts", up, down }]).run(&*conn)?;
```
Tracked in a `_migrations` table, one transaction per migration,
`rollback()` undoes the last batch. `fornthulani generate model/resource`
writes and registers these for you.

## Validation integration

Enable feature `validation` and call `register_validation_rules()` once:

```rust
req.validate([
    ("email",   "required|email|unique:users,email"),
    ("team_id", "required|integer|exists:teams,id"),
])?;
// editing: ignore the current row
("email", format!("required|email|unique:users,email,{id}"))
```
Both rules fail closed if the database cannot be queried.

## Limits (v0.1)

- SQLite only; one connection behind a mutex (fine for most JSON APIs).
- Integer primary keys. No BLOB columns. No eager loading (`with`) yet —
  use `has_many().get()`; watch for N+1.
- `Schema::has_table` is SQLite-specific.

