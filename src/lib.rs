//! Eloquent-style models, query builder and migrations for fornthulani.
//!
//! ```ignore
//! use fornthulani_plugin_orm::{Db, Model, SqliteConnection};
//!
//! Db::set(SqliteConnection::open("app.db")?);
//! let post = Post::create(&json!({"title": "Hello"}))?;
//! let page = Post::query().where_eq("published", true).latest().paginate(1, 20)?;
//! ```
//!
//! Security defaults: bound parameters only, validated identifiers,
//! mass-assignment allow-list (`FILLABLE`, empty by default), `HIDDEN`
//! columns, guarded bulk `update`/`delete`, capped pagination, and errors
//! that map to safe HTTP responses (`?` works in controllers).

mod conn;
mod error;
mod model;
mod query;
mod schema;
#[cfg(feature = "sqlite")]
mod sqlite;
mod time;
#[cfg(feature = "validation")]
mod validation;
mod value;

pub use conn::{Connection, Db, ExecResult, Executor};
pub use error::{OrmError, OrmResult};
pub use model::{Builder, MAX_PER_PAGE, Model, Page, collection_json};
pub use query::{Query, Trashed, pairs_from_json};
pub use schema::{Blueprint, ColumnDef, Migration, Migrator, Schema};
#[cfg(feature = "sqlite")]
pub use sqlite::SqliteConnection;
pub use time::now_iso;
#[cfg(feature = "validation")]
pub use validation::register_validation_rules;
pub use value::{Row, SqlValue};
