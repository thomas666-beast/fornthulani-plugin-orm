//! Eloquent-style models, relations with eager loading, query builder,
//! schema builder and migrations for fornthulani, on SQLite (feature
//! `sqlite`, default) and PostgreSQL (feature `postgres`).
//!
//! ```ignore
//! use fornthulani_plugin_orm::{Db, Model, PgConnection};
//!
//! Db::set(PgConnection::connect("postgres://app:secret@localhost/app")?);
//! let post = Post::create(&json!({"title": "Hello"}))?;
//! let page = Post::with(&["author", "comments"])
//!     .with_count(&["comments"])
//!     .where_has("tags", |q| q.where_eq("name", "rust"))
//!     .latest()
//!     .paginate(1, 20)?;
//! ```
//!
//! Security defaults: bound parameters only, validated identifiers, raw
//! SQL only from string literals, mass-assignment allow-list (`FILLABLE`,
//! empty by default, also for bulk inserts and upserts), `HIDDEN` columns
//! (stripped from loaded relations too), guarded bulk `update`/`delete`,
//! capped pagination, and errors that map to safe HTTP responses (`?`
//! works in controllers).

mod conn;
mod error;
mod model;
#[cfg(feature = "postgres")]
mod postgres;
mod query;
mod relation;
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;
mod time;
#[cfg(feature = "validation")]
mod validation;
mod value;

pub use conn::{
    Connection, Db, Dialect, ExecResult, Executor, QueryEvent, QueryListener, observe,
    transaction_in,
};
pub use error::{OrmError, OrmResult};
pub use model::{
    Builder, Cast, CursorPage, MAX_PER_PAGE, Model, Page, SimplePage, collection_json,
};
#[cfg(feature = "postgres")]
pub use postgres::{PgBuilder, PgConnection};
pub use query::{Agg, Query, Trashed, pairs_from_json};
pub use relation::{Constraint, Relation, SyncResult};
pub use schema::{Blueprint, ColumnDef, Migration, Migrator, Schema};
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteConnection;
pub use time::now_iso;
#[cfg(feature = "validation")]
pub use validation::register_validation_rules;
pub use value::{Row, SqlValue};
