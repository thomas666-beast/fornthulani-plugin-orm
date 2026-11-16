//! `unique` / `exists` validation rules on PostgreSQL (skipped without `ORM_PG_URL`).
#![cfg(all(feature = "validation", feature = "postgres"))]

mod validation_common;

use fornthulani_plugin_orm::{Db, PgConnection, Query};

#[test]
fn unique_and_exists_rules_postgres() {
    let Ok(url) = std::env::var("ORM_PG_URL") else {
        eprintln!("ORM_PG_URL not set; skipping PostgreSQL test");
        return;
    };
    let conn = PgConnection::connect(&url).unwrap();
    validation_common::reset(&conn).unwrap();
    Db::set(conn);
    // a second pool on the same database, for `exists:second.v_teams`
    Db::add_connection("second", PgConnection::connect(&url).unwrap());
    // and a PostgreSQL schema, for `exists:v_extra.plans` (not a connection name)
    Db::table("v_teams").count().unwrap();
    fornthulani_plugin_orm::Db::transaction(|ex| {
        ex.execute_batch(
            "DROP SCHEMA IF EXISTS v_extra CASCADE; CREATE SCHEMA v_extra; \
             CREATE TABLE v_extra.plans (id BIGSERIAL PRIMARY KEY, code TEXT NOT NULL)",
        )?;
        Query::new("v_extra.plans").insert_on(ex, vec![("code".into(), "pro".into())])?;
        Ok(())
    })
    .unwrap();
    validation_common::run();

    use fornthulani_plugin_validate::Validator;
    use serde_json::json;
    let ok = |v, r: &str| Validator::make(v, [("plan", r)]).validate().is_ok();
    assert!(ok(json!({"plan": "pro"}), "exists:v_extra.plans,code"));
    assert!(!ok(json!({"plan": "free"}), "exists:v_extra.plans,code"));
    assert!(!ok(json!({"plan": "pro"}), "unique:v_extra.plans,code"));
}
