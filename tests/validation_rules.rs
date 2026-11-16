#![cfg(feature = "validation")]

use fornthulani_plugin_orm::{
    Db, Executor, Migration, Migrator, OrmResult, Schema, SqliteConnection,
    register_validation_rules,
};
use fornthulani_plugin_validate::Validator;
use serde_json::json;

fn up(db: &dyn Executor) -> OrmResult<()> {
    Schema::create(db, "users", |t| {
        t.id();
        t.string("email", 255).unique();
    })
}
fn down(db: &dyn Executor) -> OrmResult<()> {
    Schema::drop_if_exists(db, "users")
}

#[test]
fn unique_and_exists_rules() {
    let conn = SqliteConnection::open_in_memory().unwrap();
    Migrator::new(vec![Migration {
        name: "1_users",
        up,
        down,
    }])
    .run(&conn)
    .unwrap();
    Db::set(conn);
    Db::table("users")
        .insert(vec![("email".into(), "a@x.io".into())])
        .unwrap();

    register_validation_rules();

    let unique = [("email", "required|email|unique:users,email")];
    assert!(
        Validator::make(json!({"email": "new@x.io"}), unique)
            .validate()
            .is_ok()
    );
    let e = Validator::make(json!({"email": "a@x.io"}), unique).errors();
    assert_eq!(
        e.get("email").unwrap()[0],
        "The email has already been taken."
    );

    // ignoring the row being edited (id 1)
    let ignoring = [("email", "required|email|unique:users,email,1")];
    assert!(
        Validator::make(json!({"email": "a@x.io"}), ignoring)
            .validate()
            .is_ok()
    );

    let exists = [("email", "exists:users,email")];
    assert!(
        Validator::make(json!({"email": "a@x.io"}), exists)
            .validate()
            .is_ok()
    );
    assert!(
        Validator::make(json!({"email": "nobody@x.io"}), exists)
            .validate()
            .is_err()
    );

    // hostile table names fail closed instead of injecting
    let evil = [("email", "unique:users;DROP TABLE users,email")];
    assert!(
        Validator::make(json!({"email": "x@x.io"}), evil)
            .validate()
            .is_err()
    );
    assert_eq!(Db::table("users").count().unwrap(), 1);
}
