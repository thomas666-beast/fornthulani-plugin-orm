//! End-to-end ORM behaviour against an in-memory SQLite database.
//! One test function, because the connection is a process-wide global.

use fornthulani::ThulaniError;
use fornthulani_plugin_orm::{
    Db, Executor, Migration, Migrator, Model, OrmError, OrmResult, Schema, SqlValue,
    SqliteConnection, collection_json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct User {
    id: Option<i64>,
    name: String,
    email: String,
    password: String,
    is_admin: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
}

impl Model for User {
    const TABLE: &'static str = "users";
    const FILLABLE: &'static [&'static str] = &["name", "email", "password"];
    const HIDDEN: &'static [&'static str] = &["password"];
    const BOOLS: &'static [&'static str] = &["is_admin"];
    fn id(&self) -> Option<i64> {
        self.id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Post {
    id: Option<i64>,
    user_id: i64,
    title: String,
    published: bool,
    created_at: Option<String>,
    updated_at: Option<String>,
    deleted_at: Option<String>,
}

impl Model for Post {
    const TABLE: &'static str = "posts";
    const SOFT_DELETES: bool = true;
    const FILLABLE: &'static [&'static str] = &["user_id", "title", "published"];
    const BOOLS: &'static [&'static str] = &["published"];
    fn id(&self) -> Option<i64> {
        self.id
    }
}

fn up_users(db: &dyn Executor) -> OrmResult<()> {
    Schema::create(db, "users", |t| {
        t.id();
        t.string("name", 100);
        t.string("email", 255).unique();
        t.string("password", 255);
        t.boolean("is_admin").default(false);
        t.timestamps();
    })
}
fn down_users(db: &dyn Executor) -> OrmResult<()> {
    Schema::drop_if_exists(db, "users")
}
fn up_posts(db: &dyn Executor) -> OrmResult<()> {
    Schema::create(db, "posts", |t| {
        t.id();
        t.integer("user_id")
            .references("users", "id")
            .on_delete_cascade();
        t.string("title", 200);
        t.boolean("published").default(false);
        t.timestamps();
        t.soft_deletes();
        t.index(&["user_id"]);
    })
}
fn down_posts(db: &dyn Executor) -> OrmResult<()> {
    Schema::drop_if_exists(db, "posts")
}

fn migrations() -> Vec<Migration> {
    vec![
        Migration {
            name: "20260101000000_create_users",
            up: up_users,
            down: down_users,
        },
        Migration {
            name: "20260101000001_create_posts",
            up: up_posts,
            down: down_posts,
        },
    ]
}

#[test]
fn orm_end_to_end() {
    let conn = SqliteConnection::open_in_memory().unwrap();
    let migrator = Migrator::new(migrations());

    // ---------- migrations ----------
    let ran = migrator.run(&conn).unwrap();
    assert_eq!(ran.len(), 2);
    assert!(migrator.run(&conn).unwrap().is_empty(), "idempotent");
    assert!(
        migrator
            .status(&conn)
            .unwrap()
            .iter()
            .all(|(_, applied)| *applied)
    );

    Db::set(conn);

    // ---------- create + mass assignment ----------
    let ada = User::create(&json!({
        "name": "Ada", "email": "ada@x.io", "password": "hash", "is_admin": true, "id": 999
    }))
    .unwrap();
    assert_eq!(ada.id, Some(1), "client-supplied id is ignored");
    assert!(!ada.is_admin, "is_admin is not fillable");
    assert!(ada.created_at.is_some() && ada.updated_at.is_some());

    // ---------- hidden columns ----------
    let public = ada.public_json();
    assert!(public.get("password").is_none());
    assert_eq!(public["email"], "ada@x.io");
    let list = collection_json(std::slice::from_ref(&ada));
    assert!(list["data"][0].get("password").is_none());

    // ---------- unique constraint -> Constraint -> 409 ----------
    let dup = User::create(&json!({"name": "Eve", "email": "ada@x.io", "password": "p"}));
    let err = dup.unwrap_err();
    assert!(matches!(err, OrmError::Constraint(_)));
    assert!(matches!(ThulaniError::from(err), ThulaniError::Conflict(_)));

    // ---------- find / not found ----------
    assert_eq!(User::find(1).unwrap().unwrap().name, "Ada");
    assert!(User::find(42).unwrap().is_none());
    let nf = User::find_or_fail(42).unwrap_err();
    assert!(matches!(ThulaniError::from(nf), ThulaniError::NotFound(_)));

    // ---------- save / update ----------
    let mut ada = User::find_or_fail(1).unwrap();
    ada.name = "Ada L.".into();
    ada.save().unwrap();
    assert_eq!(User::find_or_fail(1).unwrap().name, "Ada L.");

    ada.update(&json!({"name": "Ada Lovelace", "is_admin": true}))
        .unwrap();
    assert_eq!(ada.name, "Ada Lovelace");
    assert!(!ada.is_admin, "update() also honours FILLABLE");

    let mut fresh = User {
        id: None,
        name: "Bob".into(),
        email: "bob@x.io".into(),
        password: "p".into(),
        is_admin: true,
        created_at: None,
        updated_at: None,
    };
    fresh.save().unwrap();
    assert!(
        fresh.id.is_some() && fresh.is_admin,
        "save() writes the struct as-is"
    );

    // ---------- query builder ----------
    let evil = "x' OR '1'='1";
    assert_eq!(User::query().where_eq("name", evil).count().unwrap(), 0);
    assert_eq!(
        User::query()
            .where_in("id", [1, fresh.id.unwrap()])
            .count()
            .unwrap(),
        2
    );
    assert_eq!(
        User::query()
            .where_in("id", Vec::<i64>::new())
            .count()
            .unwrap(),
        0
    );
    assert_eq!(
        User::query()
            .where_eq("id", 1)
            .or_where("email", "=", "bob@x.io")
            .order_by("id", "desc")
            .get()
            .unwrap()
            .len(),
        2
    );
    assert!(
        User::query()
            .where_eq("name; DROP TABLE users", 1)
            .get()
            .is_err()
    );
    assert_eq!(User::all().unwrap().len(), 2, "table still there");

    // sort whitelist
    assert!(
        User::query()
            .sort_by(Some("password"), &["name"], ("id", "asc"))
            .get()
            .is_err()
    );
    let sorted = User::query()
        .sort_by(Some("-name"), &["name"], ("id", "asc"))
        .get()
        .unwrap();
    assert_eq!(sorted[0].name, "Bob");

    // ---------- guarded bulk writes ----------
    assert!(
        User::query().delete().is_err(),
        "delete without conditions is refused"
    );
    assert!(User::query().update(&json!({"name": "x"})).is_err());

    // ---------- relations, soft deletes, pagination ----------
    for i in 1..=5 {
        Post::create(&json!({"user_id": 1, "title": format!("Post {i}"), "published": i % 2 == 1}))
            .unwrap();
    }
    let ada = User::find_or_fail(1).unwrap();
    assert_eq!(ada.has_many::<Post>("user_id").count().unwrap(), 5);
    let first_post = ada
        .has_many::<Post>("user_id")
        .order_by("id", "asc")
        .first_or_fail()
        .unwrap();
    assert!(first_post.published);
    let owner: User = first_post
        .belongs_to(Some(first_post.user_id))
        .unwrap()
        .unwrap();
    assert_eq!(owner.id, Some(1));

    assert_eq!(
        Post::query().where_eq("published", true).count().unwrap(),
        3
    );

    let mut p = Post::find_or_fail(2).unwrap();
    p.delete().unwrap();
    assert!(
        Post::find(2).unwrap().is_none(),
        "soft-deleted rows are hidden"
    );
    assert_eq!(Post::query().count().unwrap(), 4);
    assert_eq!(Post::with_trashed().count().unwrap(), 5);
    assert_eq!(Post::only_trashed().count().unwrap(), 1);
    let mut trashed = Post::only_trashed().first_or_fail().unwrap();
    assert!(trashed.deleted_at.is_some());
    trashed.restore().unwrap();
    assert!(trashed.deleted_at.is_none());
    assert_eq!(Post::query().count().unwrap(), 5);
    p = Post::find_or_fail(2).unwrap();
    p.force_delete().unwrap();
    assert_eq!(Post::with_trashed().count().unwrap(), 4);

    let page = Post::query().order_by("id", "asc").paginate(2, 3).unwrap();
    assert_eq!(
        (page.total, page.page, page.per_page, page.last_page),
        (4, 2, 3, 2)
    );
    assert_eq!(page.data.len(), 1);
    assert_eq!(page.to_json()["meta"]["total"], 4);
    let huge = Post::query().paginate(1, 1_000_000).unwrap();
    assert_eq!(huge.per_page, 100, "per_page is capped");
    assert_eq!(Post::query().paginate(0, 0).unwrap().page, 1);

    // bulk update/delete with conditions
    let n = Post::query()
        .where_eq("user_id", 1)
        .update(&json!({"title": "Same"}))
        .unwrap();
    assert_eq!(n, 4);
    assert_eq!(Post::query().where_eq("title", "Same").delete().unwrap(), 4);
    assert_eq!(Post::query().count().unwrap(), 0);

    // ---------- transactions ----------
    let r: OrmResult<()> = Db::transaction(|tx| {
        User::create_on(
            tx,
            &json!({"name": "T1", "email": "t1@x.io", "password": "p"}),
        )?;
        User::create_on(
            tx,
            &json!({"name": "T2", "email": "ada@x.io", "password": "p"}),
        )?; // dup
        Ok(())
    });
    assert!(r.is_err());
    assert_eq!(
        User::query().where_eq("name", "T1").count().unwrap(),
        0,
        "rolled back"
    );

    let made = Db::transaction(|tx| {
        User::create_on(
            tx,
            &json!({"name": "T3", "email": "t3@x.io", "password": "p"}),
        )
    })
    .unwrap();
    assert_eq!(User::find(made.id.unwrap()).unwrap().unwrap().name, "T3");

    // ---------- raw SQL stays parameterised ----------
    let rows = Db::select(
        "SELECT COUNT(*) AS n FROM users WHERE name = ?",
        &[SqlValue::Text(evil.into())],
    )
    .unwrap();
    assert_eq!(rows[0]["n"], 0);

    // ---------- foreign keys are enforced ----------
    let orphan = Post::create(&json!({"user_id": 9999, "title": "orphan"}));
    assert!(matches!(orphan, Err(OrmError::Constraint(_))));

    // ---------- rollback ----------
    let conn = Db::get().unwrap();
    let undone = migrator.rollback(&*conn).unwrap();
    assert_eq!(
        undone,
        vec!["20260101000001_create_posts", "20260101000000_create_users"]
    );
    assert!(!Schema::has_table(conn.as_executor(), "users").unwrap());
}
