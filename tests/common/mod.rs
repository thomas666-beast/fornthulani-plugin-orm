//! One end-to-end scenario, run against every driver
//! (tests/eloquent_sqlite.rs, tests/eloquent_postgres.rs).

#![allow(dead_code)]

use fornthulani::ThulaniError;
use fornthulani_plugin_orm::{
    Cast, Db, Dialect, Executor, Model, OrmError, OrmResult, Query, QueryEvent, Relation, Schema,
    SqlValue, collection_json,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------- models

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Country {
    pub id: Option<i64>,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posts: Option<Vec<Post>>,
}

impl Model for Country {
    const TABLE: &'static str = "countries";
    const TIMESTAMPS: bool = false;
    const FILLABLE: &'static [&'static str] = &["name"];
    fn id(&self) -> Option<i64> {
        self.id
    }
    fn relations() -> Vec<Relation> {
        vec![Relation::has_many_through::<Post>(
            "posts",
            "users",
            "country_id",
            "user_id",
        )]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Option<i64>,
    pub country_id: Option<i64>,
    pub name: String,
    pub email: String,
    pub password: String,
    pub is_admin: bool,
    pub settings: Value,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posts: Option<Vec<Post>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub posts_count: Option<i64>,
}

impl Model for User {
    const TABLE: &'static str = "users";
    const FILLABLE: &'static [&'static str] =
        &["name", "email", "password", "country_id", "settings"];
    const HIDDEN: &'static [&'static str] = &["password"];
    const CASTS: &'static [(&'static str, Cast)] =
        &[("is_admin", Cast::Bool), ("settings", Cast::Json)];
    fn id(&self) -> Option<i64> {
        self.id
    }
    fn relations() -> Vec<Relation> {
        vec![Relation::has_many::<Post>("posts", "user_id")]
    }
    fn creating(attrs: &mut Map<String, Value>, _ex: &dyn Executor) -> OrmResult<()> {
        attrs.entry("settings").or_insert(json!({}));
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: Option<i64>,
    pub user_id: i64,
    pub title: String,
    pub slug: String,
    pub published: bool,
    pub views: i64,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub deleted_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Box<User>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments: Option<Vec<Comment>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<Tag>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments_count: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments_sum_likes: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags_exists: Option<bool>,
}

fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

pub static DELETED_POSTS: AtomicUsize = AtomicUsize::new(0);

impl Model for Post {
    const TABLE: &'static str = "posts";
    const SOFT_DELETES: bool = true;
    const FILLABLE: &'static [&'static str] = &["user_id", "title", "published", "views"];
    const BOOLS: &'static [&'static str] = &["published"];
    fn id(&self) -> Option<i64> {
        self.id
    }
    fn relations() -> Vec<Relation> {
        vec![
            Relation::belongs_to::<User>("author", "user_id"),
            Relation::has_many::<Comment>("comments", "post_id"),
            Relation::belongs_to_many::<Tag>("tags", "post_tag", "post_id", "tag_id")
                .with_pivot(&["role"]),
        ]
    }
    fn creating(attrs: &mut Map<String, Value>, _ex: &dyn Executor) -> OrmResult<()> {
        let title = attrs
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let slug = slugify(title);
        attrs.insert("slug".into(), json!(slug));
        Ok(())
    }
    fn deleted(&self, _ex: &dyn Executor) -> OrmResult<()> {
        DELETED_POSTS.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub id: Option<i64>,
    pub post_id: i64,
    pub body: String,
    pub likes: i64,
    pub approved: bool,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post: Option<Box<Post>>,
}

impl Model for Comment {
    const TABLE: &'static str = "comments";
    const FILLABLE: &'static [&'static str] = &["post_id", "body", "likes", "approved"];
    const BOOLS: &'static [&'static str] = &["approved"];
    fn id(&self) -> Option<i64> {
        self.id
    }
    fn relations() -> Vec<Relation> {
        vec![Relation::belongs_to::<Post>("post", "post_id")]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tag {
    pub id: Option<i64>,
    pub name: String,
    pub hidden: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pivot: Option<Value>,
}

impl Model for Tag {
    const TABLE: &'static str = "tags";
    const TIMESTAMPS: bool = false;
    const FILLABLE: &'static [&'static str] = &["name", "hidden"];
    const BOOLS: &'static [&'static str] = &["hidden"];
    fn id(&self) -> Option<i64> {
        self.id
    }
    // a global scope: hidden tags never show up unless asked for
    fn global_scope(q: Query) -> Query {
        q.where_eq("hidden", false)
    }
}

// ---------------------------------------------------------------- schema

pub fn reset_schema(db: &dyn Executor) -> OrmResult<()> {
    for t in [
        "post_tag",
        "comments",
        "tags",
        "posts",
        "users",
        "countries",
    ] {
        Schema::drop_if_exists(db, t)?;
    }
    Schema::create(db, "countries", |t| {
        t.id();
        t.string("name", 100);
    })?;
    Schema::create(db, "users", |t| {
        t.id();
        t.foreign_id("country_id")
            .nullable()
            .constrained("countries")
            .null_on_delete();
        t.string("name", 100);
        t.string("email", 255).unique();
        t.string("password", 255);
        t.boolean("is_admin").default(false);
        t.json("settings").nullable();
        t.timestamps();
    })?;
    Schema::create(db, "posts", |t| {
        t.id();
        t.foreign_id("user_id")
            .constrained("users")
            .cascade_on_delete();
        t.string("title", 200);
        t.string("slug", 200);
        t.boolean("published").default(false);
        t.big_integer("views").default(0);
        t.timestamps();
        t.soft_deletes();
        t.index(&["user_id", "published"]);
    })?;
    Schema::create(db, "comments", |t| {
        t.id();
        t.foreign_id("post_id")
            .constrained("posts")
            .cascade_on_delete();
        t.text("body");
        t.integer("likes").default(0);
        t.boolean("approved").default(false);
        t.timestamps();
    })?;
    Schema::create(db, "tags", |t| {
        t.id();
        t.string("name", 50).unique();
        t.boolean("hidden").default(false);
    })?;
    Schema::create(db, "post_tag", |t| {
        t.foreign_id("post_id")
            .constrained("posts")
            .cascade_on_delete();
        t.foreign_id("tag_id")
            .constrained("tags")
            .cascade_on_delete();
        t.string("role", 20).nullable();
        t.unique_index(&["post_id", "tag_id"]);
    })?;
    Ok(())
}

static QUERIES: AtomicUsize = AtomicUsize::new(0);
fn count_queries(_e: &QueryEvent<'_>) {
    QUERIES.fetch_add(1, Ordering::SeqCst);
}

fn id(v: &Option<i64>) -> i64 {
    v.expect("saved model has an id")
}

// ---------------------------------------------------------------- scenario

pub fn run(dialect: Dialect) {
    let db_dialect = Db::transaction(|ex| Ok(ex.dialect())).unwrap();
    assert_eq!(db_dialect, dialect);

    // ---------- schema introspection / alter ----------
    Db::transaction(|ex| {
        assert!(Schema::has_table(ex, "posts")?);
        assert!(!Schema::has_table(ex, "nope")?);
        assert!(Schema::has_column(ex, "posts", "slug")?);
        Schema::table(ex, "posts", |t| {
            t.string("subtitle", 100).nullable();
        })?;
        assert!(Schema::has_column(ex, "posts", "subtitle")?);
        Schema::table(ex, "posts", |t| t.drop_column("subtitle"))?;
        assert!(!Schema::has_column(ex, "posts", "subtitle")?);
        Ok(())
    })
    .unwrap();

    // ---------- create, casts, hooks, mass assignment ----------
    let dk = Country::create(&json!({"name": "Denmark"})).unwrap();
    let rs = Country::create(&json!({"name": "Serbia"})).unwrap();

    let ada = User::create(&json!({
        "name": "Ada", "email": "ada@x.io", "password": "hash", "country_id": dk.id,
        "settings": {"theme": "dark", "notify": {"email": true}},
        "is_admin": true, "id": 999_999
    }))
    .unwrap();
    assert_ne!(ada.id, Some(999_999), "id is not fillable");
    assert!(!ada.is_admin, "is_admin is not fillable");
    assert_eq!(ada.settings["theme"], "dark", "json cast round-trips");
    assert!(ada.created_at.is_some());

    let bob = User::create(
        &json!({"name": "Bob", "email": "bob@x.io", "password": "h", "country_id": rs.id}),
    )
    .unwrap();
    assert_eq!(bob.settings, json!({}), "creating hook filled a default");
    let cy = User::create(&json!({"name": "Cy", "email": "cy@x.io", "password": "h"})).unwrap();

    // duplicate email -> Constraint -> 409
    let dup =
        User::create(&json!({"name": "A2", "email": "ada@x.io", "password": "h"})).unwrap_err();
    assert!(matches!(dup, OrmError::Constraint(_)), "{dup:?}");
    assert!(matches!(ThulaniError::from(dup), ThulaniError::Conflict(_)));

    let p1 = Post::create(
        &json!({"user_id": ada.id, "title": "Hello World", "published": true, "views": 10}),
    )
    .unwrap();
    assert_eq!(p1.slug, "hello-world", "creating hook set the slug");
    assert!(p1.published);
    let p2 = Post::create(&json!({"user_id": ada.id, "title": "Draft", "views": 3})).unwrap();
    let p3 = Post::create(
        &json!({"user_id": bob.id, "title": "Bob's post", "published": true, "views": 7}),
    )
    .unwrap();

    // FK violation -> Constraint
    let fk = Post::create(&json!({"user_id": 987_654, "title": "orphan"})).unwrap_err();
    assert!(matches!(fk, OrmError::Constraint(_)), "{fk:?}");

    for (post, body, likes, ok) in [
        (&p1, "great", 5, true),
        (&p1, "meh", 1, false),
        (&p1, "nice", 2, true),
        (&p3, "first", 0, true),
    ] {
        Comment::create(&json!({"post_id": post.id, "body": body, "likes": likes, "approved": ok}))
            .unwrap();
    }

    let rust = Tag::create(&json!({"name": "rust"})).unwrap();
    let web = Tag::create(&json!({"name": "web"})).unwrap();
    let secret = Tag::create(&json!({"name": "secret", "hidden": true})).unwrap();

    // ---------- global scope ----------
    assert_eq!(Tag::all().unwrap().len(), 2);
    assert_eq!(Tag::query().without_global_scope().count().unwrap(), 3);
    assert!(Tag::find(id(&secret.id)).unwrap().is_none(), "scoped out");

    // ---------- pivots: attach / sync / detach ----------
    assert_eq!(
        p1.attach_with("tags", &[id(&rust.id)], &json!({"role": "primary"}))
            .unwrap(),
        1
    );
    assert_eq!(
        p1.attach("tags", &[id(&rust.id), id(&web.id)]).unwrap(),
        1,
        "already attached is skipped"
    );
    p1.attach("tags", &[id(&secret.id)]).unwrap();
    let s = p3.sync("tags", &[id(&web.id), id(&rust.id)]).unwrap();
    assert_eq!(s.attached.len(), 2);
    let s = p3.sync("tags", &[id(&web.id)]).unwrap();
    assert_eq!((s.attached.len(), s.detached.len()), (0, 1));

    // ---------- eager loading (nested, constrained), counts ----------
    QUERIES.store(0, Ordering::SeqCst);
    Db::listen(count_queries);
    let users = User::with(&["posts.comments", "posts.tags"])
        .with_where("posts", |q| q.where_eq("published", true))
        .with_count(&["posts"])
        .order_by("id", "asc")
        .get()
        .unwrap();
    Db::stop_listening();
    assert_eq!(
        QUERIES.load(Ordering::SeqCst),
        5,
        "users + posts + comments + pivot + tags: no N+1"
    );
    assert_eq!(users.len(), 3);
    let a = &users[0];
    assert_eq!(a.posts_count, Some(2), "count ignores the eager constraint");
    let a_posts = a.posts.as_ref().unwrap();
    assert_eq!(a_posts.len(), 1, "only published posts loaded");
    assert_eq!(a_posts[0].comments.as_ref().unwrap().len(), 3);
    let tags = a_posts[0].tags.as_ref().unwrap();
    assert_eq!(
        tags.len(),
        2,
        "hidden tag removed by the related model's global scope"
    );
    let rust_tag = tags.iter().find(|t| t.name == "rust").unwrap();
    assert_eq!(rust_tag.pivot.as_ref().unwrap()["role"], "primary");
    assert_eq!(
        users[2].posts.as_ref().unwrap().len(),
        0,
        "empty relation is []"
    );

    // hidden attributes stripped recursively
    let public = collection_json(&users);
    assert!(public["data"][0].get("password").is_none());
    let mut p = Post::with(&["author"])
        .where_eq("id", id(&p1.id))
        .first()
        .unwrap()
        .unwrap();
    assert_eq!(p.author.as_ref().unwrap().name, "Ada");
    assert!(p.public_json()["author"].get("password").is_none());

    // lazy eager load and related()
    p.load(&["comments"]).unwrap();
    assert_eq!(p.comments.as_ref().unwrap().len(), 3);
    let approved = p
        .related::<Comment>("comments")
        .where_eq("approved", true)
        .count()
        .unwrap();
    assert_eq!(approved, 2);
    let bob_posts = bob.related::<Post>("posts").get().unwrap();
    assert_eq!(bob_posts.len(), 1);

    // belongs_to eager, inverse
    let comments = Comment::with(&["post.author"]).get().unwrap();
    assert!(
        comments
            .iter()
            .all(|c| c.post.as_ref().unwrap().author.is_some())
    );

    // has_many_through
    let countries = Country::with(&["posts"])
        .order_by("id", "asc")
        .get()
        .unwrap();
    assert_eq!(countries[0].posts.as_ref().unwrap().len(), 2);
    assert_eq!(countries[1].posts.as_ref().unwrap().len(), 1);

    // with_sum / with_count_where / with_exists
    let posts = Post::query()
        .with_count_where("comments", |q| q.where_eq("approved", true))
        .with_sum("comments", "likes")
        .with_exists(&["tags"])
        .order_by("id", "asc")
        .get()
        .unwrap();
    assert_eq!(posts[0].comments_count, Some(2));
    assert_eq!(posts[0].comments_sum_likes, Some(8.0));
    assert_eq!(posts[0].tags_exists, Some(true));
    assert_eq!(
        posts[1].comments_sum_likes, None,
        "SUM over nothing is NULL"
    );
    assert_eq!(posts[1].tags_exists, Some(false));

    // ---------- relationship existence ----------
    assert_eq!(User::query().has("posts").count().unwrap(), 2);
    assert_eq!(
        User::query()
            .doesnt_have("posts")
            .first()
            .unwrap()
            .unwrap()
            .name,
        "Cy"
    );
    let liked = Post::query()
        .where_has("comments", |q| q.where_("likes", ">=", 5))
        .get()
        .unwrap();
    assert_eq!(liked.len(), 1);
    assert_eq!(liked[0].id, p1.id);
    let nested = User::query()
        .where_has("posts", |q| q.where_like("title", "%World%"))
        .or_where_has("posts", |q| q.where_eq("views", 7))
        .count()
        .unwrap();
    assert_eq!(nested, 2);

    // ---------- aggregates / pluck / value ----------
    assert_eq!(Post::query().sum("views").unwrap(), 20.0);
    assert!((Post::query().avg("views").unwrap().unwrap() - 20.0 / 3.0).abs() < 1e-9);
    assert_eq!(Post::query().max("views").unwrap(), json!(10));
    assert_eq!(Post::query().min("title").unwrap(), json!("Bob's post"));
    let titles = Post::query()
        .order_by("title", "asc")
        .pluck("title")
        .unwrap();
    assert_eq!(
        titles,
        vec![json!("Bob's post"), json!("Draft"), json!("Hello World")]
    );
    assert_eq!(
        User::query()
            .where_eq("name", "Bob")
            .value("email")
            .unwrap(),
        Some(json!("bob@x.io"))
    );

    // ---------- json columns ----------
    assert_eq!(
        User::query()
            .where_json("settings->theme", "=", "dark")
            .count()
            .unwrap(),
        1
    );
    assert_eq!(
        User::query()
            .where_json("settings->notify->email", "=", true)
            .count()
            .unwrap(),
        1
    );

    // ---------- joins, group by, raw selects ----------
    let rows = Db::table("posts")
        .join("users", "users.id", "=", "posts.user_id")
        .select(["users.name"])
        .select_raw("COUNT(*) AS n, SUM(posts.views) AS total", vec![])
        .group_by(["users.name"])
        .having_raw("COUNT(*) >= ?", vec![SqlValue::Int(1)])
        .order_by("users.name", "asc")
        .get()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["name"], "Ada");
    assert_eq!(rows[0]["n"], 2);
    assert_eq!(rows[0]["total"], 13);
    assert_eq!(Db::table("posts").group_by(["user_id"]).count().unwrap(), 2);

    // ---------- increment / update / touch ----------
    let mut p = Post::find(id(&p2.id)).unwrap().unwrap();
    p.increment("views", 5).unwrap();
    assert_eq!(p.views, 8);
    p.decrement("views", 1).unwrap();
    assert_eq!(Post::find(id(&p2.id)).unwrap().unwrap().views, 7);
    assert_eq!(
        Post::query()
            .where_eq("published", false)
            .increment("views", 1)
            .unwrap(),
        1
    );
    p.update(&json!({"title": "Draft v2", "slug": "ignored-not-fillable"}))
        .unwrap();
    let p = Post::find(id(&p2.id)).unwrap().unwrap();
    assert_eq!((p.title.as_str(), p.slug.as_str()), ("Draft v2", "draft"));

    // ---------- first_or_create / update_or_create ----------
    let t = Tag::first_or_create(&json!({"name": "rust"}), &json!({})).unwrap();
    assert_eq!(t.id, rust.id);
    let t = Tag::first_or_create(&json!({"name": "db"}), &json!({"hidden": false})).unwrap();
    assert!(t.id.is_some());
    let u =
        User::update_or_create(&json!({"email": "cy@x.io"}), &json!({"name": "Cyrus"})).unwrap();
    assert_eq!((u.id, u.name.as_str()), (cy.id, "Cyrus"));

    // ---------- bulk insert / upsert ----------
    let n = Tag::insert_many(&[
        json!({"name": "a"}),
        json!({"name": "b"}),
        json!({"name": "c"}),
    ])
    .unwrap();
    assert_eq!(n, 3);
    Tag::upsert(
        &[
            json!({"name": "a", "hidden": true}),
            json!({"name": "z", "hidden": false}),
        ],
        &["name"],
        &["hidden"],
    )
    .unwrap();
    assert!(
        Tag::query()
            .without_global_scope()
            .where_eq("name", "a")
            .first()
            .unwrap()
            .unwrap()
            .hidden
    );
    assert_eq!(
        Tag::query()
            .where_in("name", ["a", "b", "c", "z"])
            .count()
            .unwrap(),
        3
    );

    // ---------- chunking / pagination ----------
    let mut seen = 0;
    Tag::query()
        .without_global_scope()
        .chunk_by_id(2, |chunk| {
            assert!(chunk.len() <= 2);
            seen += chunk.len();
            Ok(true)
        })
        .unwrap();
    assert_eq!(
        seen as u64,
        Tag::query().without_global_scope().count().unwrap()
    );

    let page = Tag::query().order_by("id", "asc").paginate(1, 2).unwrap();
    assert_eq!((page.data.len(), page.per_page), (2, 2));
    assert!(page.last_page >= 3);
    let simple = Tag::query()
        .order_by("id", "asc")
        .simple_paginate(1, 2)
        .unwrap();
    assert!(simple.has_more);
    let mut cursor: Option<String> = None;
    let mut collected = Vec::new();
    loop {
        let c = Tag::query().cursor_paginate(2, cursor.as_deref()).unwrap();
        collected.extend(c.data.iter().map(|t| t.id.unwrap()));
        match c.next_cursor {
            Some(n) => cursor = Some(n),
            None => break,
        }
    }
    let mut sorted = collected.clone();
    sorted.sort();
    assert_eq!(collected, sorted, "cursor pages are ordered by id");
    assert_eq!(collected.len() as u64, Tag::query().count().unwrap());
    assert!(
        Tag::query().cursor_paginate(2, Some("garbage!")).is_err(),
        "bad cursors are rejected"
    );

    // ---------- soft deletes + hooks ----------
    let before = DELETED_POSTS.load(Ordering::SeqCst);
    p.delete().unwrap();
    assert_eq!(
        DELETED_POSTS.load(Ordering::SeqCst),
        before + 1,
        "deleted hook ran"
    );
    assert!(Post::find(id(&p2.id)).unwrap().is_none());
    assert_eq!(Post::with_trashed().count().unwrap(), 3);
    assert_eq!(Post::only_trashed().count().unwrap(), 1);
    assert_eq!(
        User::query()
            .has("posts")
            .where_eq("name", "Ada")
            .count()
            .unwrap(),
        1
    );
    let ada_live = User::with(&["posts"])
        .where_eq("id", id(&ada.id))
        .first()
        .unwrap()
        .unwrap();
    assert_eq!(
        ada_live.posts.unwrap().len(),
        1,
        "trashed posts are not eager loaded"
    );
    let mut trashed = Post::only_trashed().first().unwrap().unwrap();
    trashed.restore().unwrap();
    assert_eq!(Post::query().count().unwrap(), 3);

    // ---------- transactions ----------
    let r: OrmResult<()> = Db::transaction(|ex| {
        let mut locked = Post::query()
            .where_eq("id", id(&p1.id))
            .lock_for_update()
            .first_on(ex)?
            .unwrap();
        locked.save_on(ex)?;
        Comment::create_on(ex, &json!({"post_id": p1.id, "body": "in tx"}))?;
        Err(OrmError::Invalid("boom".into()))
    });
    assert!(r.is_err());
    assert_eq!(
        Comment::query().where_eq("body", "in tx").count().unwrap(),
        0,
        "rolled back"
    );
    Db::transaction(|ex| Comment::create_on(ex, &json!({"post_id": p1.id, "body": "kept"})))
        .unwrap();
    assert_eq!(
        Comment::query().where_eq("body", "kept").count().unwrap(),
        1
    );

    // ---------- sole / errors ----------
    assert!(
        matches!(User::query().sole(), Err(OrmError::Invalid(_))),
        "more than one row"
    );
    assert_eq!(
        User::query()
            .where_eq("email", "bob@x.io")
            .sole()
            .unwrap()
            .name,
        "Bob"
    );
    assert!(matches!(
        User::find_or_fail(987_654),
        Err(OrmError::NotFound(_))
    ));
    let bad = Db::table("users; DROP TABLE users").count();
    assert!(bad.is_err(), "hostile identifiers rejected");

    // ---------- replicate, destroy, detach, cascade ----------
    let copy = p1.replicate().unwrap();
    assert!(copy.id.is_none());
    assert_eq!(p1.detach("tags", &[]).unwrap(), 3);
    assert_eq!(Post::destroy(&[id(&p3.id)]).unwrap(), 1);
    p3.force_delete().unwrap();
    assert_eq!(
        Comment::query()
            .where_eq("post_id", id(&p3.id))
            .count()
            .unwrap(),
        0,
        "FK cascade"
    );
}
