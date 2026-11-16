//! `unique` / `exists` validation rules on SQLite.
#![cfg(all(feature = "validation", feature = "sqlite"))]

mod validation_common;

use fornthulani_plugin_orm::{Db, SqliteConnection};

#[test]
fn unique_and_exists_rules_sqlite() {
    let conn = SqliteConnection::open_in_memory().unwrap();
    validation_common::reset(&conn).unwrap();
    Db::set(conn);
    // a named connection pointing at the same data shape
    let second = SqliteConnection::open_in_memory().unwrap();
    validation_common::reset(&second).unwrap();
    fornthulani_plugin_orm::Query::new("v_teams")
        .insert_on(&second, vec![("code".into(), "red".into())])
        .unwrap();
    Db::add_connection("second", second);
    validation_common::run();
}
