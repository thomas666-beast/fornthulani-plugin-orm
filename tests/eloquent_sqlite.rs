//! The shared Eloquent-style scenario on SQLite.
#![cfg(feature = "sqlite")]

mod common;

use fornthulani_plugin_orm::{Db, Dialect, SqliteConnection};

#[test]
fn eloquent_on_sqlite() {
    let conn = SqliteConnection::open_in_memory().unwrap();
    common::reset_schema(&conn).unwrap();
    Db::set(conn);
    common::run(Dialect::Sqlite);
}
