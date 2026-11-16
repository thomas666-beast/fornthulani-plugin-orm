//! The shared Eloquent-style scenario on PostgreSQL.
//!
//! Needs a scratch database: `ORM_PG_URL=postgres://postgres:postgres@localhost/orm_test cargo test --all-features`.
//! Skipped (passes) when `ORM_PG_URL` is not set.
#![cfg(feature = "postgres")]

mod common;

use fornthulani_plugin_orm::{
    Connection, Db, Dialect, Executor, OrmError, PgConnection, Query, SqlValue,
};
use std::sync::Arc;

#[test]
fn eloquent_on_postgres() {
    let Ok(url) = std::env::var("ORM_PG_URL") else {
        eprintln!("ORM_PG_URL not set; skipping PostgreSQL test");
        return;
    };
    let conn = PgConnection::builder(&url)
        .max_connections(4)
        .connect()
        .unwrap();
    common::reset_schema(&conn).unwrap();
    Db::set(conn);
    common::run(Dialect::Postgres);
}

#[test]
fn postgres_pool_and_errors() {
    let Ok(url) = std::env::var("ORM_PG_URL") else {
        return;
    };
    let pool = Arc::new(
        PgConnection::builder(&url)
            .max_connections(2)
            .connect()
            .unwrap(),
    );

    // 8 threads share 2 connections
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let pool = pool.clone();
            std::thread::spawn(move || {
                let rows = pool
                    .query(
                        "SELECT ?::bigint * 2 AS n, pg_sleep(0.02)::text AS s",
                        &[SqlValue::Int(i)],
                    )
                    .unwrap();
                assert_eq!(rows[0]["n"], i * 2);
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(pool.connections() <= 2);

    // bad input for a typed column -> Invalid (400), not a 500
    let e = Query::new("pg_class")
        .where_eq("oid", "not-a-number")
        .count_on(&*pool)
        .unwrap_err();
    assert!(matches!(e, OrmError::Invalid(_)), "{e:?}");

    // types decode to natural JSON
    let r = &pool
        .query(
            "SELECT 19.99::numeric AS price, 12345678901234567890::numeric AS big, \
             '2026-03-01 12:30:00.25+00'::timestamptz AS at, '2026-03-01'::date AS d, \
             'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid AS u, '{\"a\":1}'::jsonb AS j, \
             ARRAY[1,2,NULL]::int[] AS arr, '\\x0102'::bytea AS b, '10.0.0.1'::inet AS ip, \
             ? = 1 AS bool_param",
            &[SqlValue::Int(1)],
        )
        .unwrap()[0];
    assert_eq!(r["price"], 19.99);
    assert_eq!(r["big"], "12345678901234567890");
    assert_eq!(r["at"], "2026-03-01T12:30:00.25Z");
    assert_eq!(r["d"], "2026-03-01");
    assert_eq!(r["u"], "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    assert_eq!(r["j"]["a"], 1);
    assert_eq!(r["arr"], serde_json::json!([1, 2, null]));
    assert_eq!(r["b"], "\\x0102");
    assert_eq!(r["ip"], "10.0.0.1");
    assert_eq!(r["bool_param"], true);

    // a broken transaction is rolled back and the connection reused
    let r: Result<(), _> = pool.transaction(&mut |ex: &dyn Executor| {
        ex.execute_batch("CREATE TEMP TABLE tx_probe (x int)")?;
        ex.query("SELECT 1/0 AS boom", &[]).map(|_| ())
    });
    assert!(r.is_err());
    assert_eq!(pool.query("SELECT 1 AS one", &[]).unwrap()[0]["one"], 1);
}
