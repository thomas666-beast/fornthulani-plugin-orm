//! `unique` / `exists` rules, run against every driver.

use fornthulani_plugin_orm::{Db, Executor, OrmResult, Schema, register_validation_rules};
use fornthulani_plugin_validate::Validator;
use serde_json::{Value, json};

pub fn reset(db: &dyn Executor) -> OrmResult<()> {
    Schema::drop_if_exists(db, "v_members")?;
    Schema::drop_if_exists(db, "v_teams")?;
    Schema::create(db, "v_teams", |t| {
        t.id();
        t.string("code", 20).unique();
        t.boolean("active").default(true);
    })?;
    Schema::create(db, "v_members", |t| {
        t.id();
        t.foreign_id("team_id").constrained("v_teams");
        t.string("email", 255);
        t.timestamp("deleted_at").nullable();
    })
}

fn ok(data: Value, rules: &[(&str, &str)]) -> bool {
    Validator::make(data, rules).validate().is_ok()
}

pub fn run() {
    register_validation_rules();
    let t1 = Db::table("v_teams")
        .insert_get_id(vec![("code".into(), "red".into())], "id")
        .unwrap();
    let t2 = Db::table("v_teams")
        .insert_get_id(
            vec![
                ("code".into(), "blue".into()),
                ("active".into(), false.into()),
            ],
            "id",
        )
        .unwrap();
    let t1 = t1.as_i64().unwrap();
    let t2 = t2.as_i64().unwrap();
    let m1 = Db::table("v_members")
        .insert_get_id(
            vec![
                ("team_id".into(), t1.into()),
                ("email".into(), "a@x.io".into()),
            ],
            "id",
        )
        .unwrap()
        .as_i64()
        .unwrap();
    Db::table("v_members")
        .insert(vec![
            ("team_id".into(), t2.into()),
            ("email".into(), "gone@x.io".into()),
            ("deleted_at".into(), "2026-01-01T00:00:00Z".into()),
        ])
        .unwrap();

    // unique: basic, column defaults to the attribute
    assert!(ok(
        json!({"email": "new@x.io"}),
        &[("email", "required|email|unique:v_members")]
    ));
    let e = Validator::make(
        json!({"email": "a@x.io"}),
        [("email", "unique:v_members,email")],
    )
    .errors();
    assert_eq!(
        e.get("email").unwrap()[0],
        "The email has already been taken."
    );

    // unique: ignore the row being edited, NULL = ignore nothing
    assert!(ok(
        json!({"email": "a@x.io"}),
        &[("email", format!("unique:v_members,email,{m1}").as_str())]
    ));
    assert!(!ok(
        json!({"email": "a@x.io"}),
        &[("email", "unique:v_members,email,NULL")]
    ));

    // unique: extra where - soft-deleted rows do not count
    assert!(!ok(
        json!({"email": "gone@x.io"}),
        &[("email", "unique:v_members,email")]
    ));
    assert!(ok(
        json!({"email": "gone@x.io"}),
        &[("email", "unique:v_members,email,NULL,id,deleted_at,NULL")]
    ));
    // odd number of where args fails closed
    assert!(!ok(
        json!({"email": "zz@x.io"}),
        &[("email", "unique:v_members,email,NULL,id,deleted_at")]
    ));

    // unique: string input against an integer column (form data)
    assert!(!ok(
        json!({"team": t1.to_string()}),
        &[("team", "unique:v_teams,id")]
    ));

    // exists: basic, typed where, negation, nested attribute
    assert!(ok(
        json!({"team_id": t1}),
        &[("team_id", "exists:v_teams,id")]
    ));
    assert!(ok(
        json!({"team_id": t1.to_string()}),
        &[("team_id", "exists:v_teams,id")]
    ));
    assert!(!ok(
        json!({"team_id": 987_654}),
        &[("team_id", "exists:v_teams,id")]
    ));
    assert!(ok(
        json!({"code": "red"}),
        &[("code", "exists:v_teams,code,active,true")]
    ));
    assert!(!ok(
        json!({"code": "blue"}),
        &[("code", "exists:v_teams,code,active,true")]
    ));
    assert!(ok(
        json!({"code": "blue"}),
        &[("code", "exists:v_teams,code,code,!red")]
    ));
    assert!(ok(
        json!({"team": {"code": "red"}}),
        &[("team.code", "exists:v_teams")]
    ));

    // exists: arrays (every element must exist, duplicates fine)
    assert!(ok(
        json!({"teams": [t1, t2, t1]}),
        &[("teams", "array|exists:v_teams,id")]
    ));
    assert!(!ok(
        json!({"teams": [t1, 987_654]}),
        &[("teams", "array|exists:v_teams,id")]
    ));

    // objects / null never pass unique
    assert!(!ok(
        json!({"email": {"$ne": 1}}),
        &[("email", "unique:v_members,email")]
    ));

    // named connection prefix
    assert!(ok(
        json!({"code": "red"}),
        &[("code", "exists:second.v_teams,code")]
    ));

    // hostile identifiers fail closed instead of injecting
    assert!(!ok(
        json!({"email": "x@x.io"}),
        &[("email", "unique:v_members;DROP TABLE v_members,email")]
    ));
    assert!(!ok(
        json!({"email": "x@x.io"}),
        &[("email", "unique:v_members,email\" OR 1=1 --")]
    ));
    assert!(!ok(
        json!({"code": "x"}),
        &[("code", "exists:no_such_table,code")]
    ));
    assert_eq!(Db::table("v_members").count().unwrap(), 2);
}
