//! Schema builder and migrations (Laravel's `Schema::create` / `migrate`).
//!
//! ```ignore
//! pub fn up(db: &dyn Executor) -> OrmResult<()> {
//!     Schema::create(db, "posts", |t| {
//!         t.id();
//!         t.string("title", 200);
//!         t.text("body");
//!         t.boolean("published").default(false);
//!         t.integer("user_id").references("users", "id").on_delete_cascade();
//!         t.timestamps();
//!         t.soft_deletes();
//!         t.index(&["user_id"]);
//!     })
//! }
//! pub fn down(db: &dyn Executor) -> OrmResult<()> { Schema::drop_if_exists(db, "posts") }
//! ```

use crate::conn::{Connection, Executor};
use crate::error::{OrmError, OrmResult};
use crate::query::quote_ident;
use crate::time::now_iso;
use crate::value::SqlValue;

#[derive(Debug, Clone)]
pub struct ColumnDef {
    name: String,
    ty: String,
    nullable: bool,
    unique: bool,
    primary: bool,
    default: Option<SqlValue>,
    references: Option<(String, String)>,
    on_delete_cascade: bool,
}

impl ColumnDef {
    fn new(name: &str, ty: &str) -> Self {
        ColumnDef {
            name: name.to_string(),
            ty: ty.to_string(),
            nullable: false,
            unique: false,
            primary: false,
            default: None,
            references: None,
            on_delete_cascade: false,
        }
    }

    /// Allow NULL (columns are `NOT NULL` by default).
    pub fn nullable(&mut self) -> &mut Self {
        self.nullable = true;
        self
    }

    pub fn unique(&mut self) -> &mut Self {
        self.unique = true;
        self
    }

    pub fn default(&mut self, v: impl Into<SqlValue>) -> &mut Self {
        self.default = Some(v.into());
        self
    }

    /// `FOREIGN KEY (this) REFERENCES table(column)`.
    pub fn references(&mut self, table: &str, column: &str) -> &mut Self {
        self.references = Some((table.to_string(), column.to_string()));
        self
    }

    pub fn on_delete_cascade(&mut self) -> &mut Self {
        self.on_delete_cascade = true;
        self
    }
}

#[derive(Debug, Default)]
pub struct Blueprint {
    columns: Vec<ColumnDef>,
    indexes: Vec<(Vec<String>, bool)>,
}

impl Blueprint {
    fn push(&mut self, name: &str, ty: &str) -> &mut ColumnDef {
        self.columns.push(ColumnDef::new(name, ty));
        let last = self.columns.len() - 1;
        &mut self.columns[last]
    }

    /// `id INTEGER PRIMARY KEY AUTOINCREMENT`.
    pub fn id(&mut self) -> &mut ColumnDef {
        let c = self.push("id", "INTEGER");
        c.primary = true;
        c
    }

    pub fn string(&mut self, name: &str, len: u32) -> &mut ColumnDef {
        self.push(name, &format!("VARCHAR({len})"))
    }
    pub fn text(&mut self, name: &str) -> &mut ColumnDef {
        self.push(name, "TEXT")
    }
    pub fn integer(&mut self, name: &str) -> &mut ColumnDef {
        self.push(name, "INTEGER")
    }
    pub fn big_integer(&mut self, name: &str) -> &mut ColumnDef {
        self.push(name, "BIGINT")
    }
    pub fn float(&mut self, name: &str) -> &mut ColumnDef {
        self.push(name, "REAL")
    }
    pub fn boolean(&mut self, name: &str) -> &mut ColumnDef {
        self.push(name, "BOOLEAN")
    }
    /// ISO-8601 text, which is what the model layer writes.
    pub fn timestamp(&mut self, name: &str) -> &mut ColumnDef {
        self.push(name, "TEXT")
    }

    /// Nullable `created_at` and `updated_at`.
    pub fn timestamps(&mut self) {
        self.timestamp("created_at").nullable();
        self.timestamp("updated_at").nullable();
    }

    /// Nullable `deleted_at`, for models with `SOFT_DELETES`.
    pub fn soft_deletes(&mut self) {
        self.timestamp("deleted_at").nullable();
    }

    pub fn index(&mut self, cols: &[&str]) {
        self.indexes
            .push((cols.iter().map(|c| c.to_string()).collect(), false));
    }

    pub fn unique_index(&mut self, cols: &[&str]) {
        self.indexes
            .push((cols.iter().map(|c| c.to_string()).collect(), true));
    }

    fn to_sql(&self, table: &str) -> OrmResult<Vec<String>> {
        if self.columns.is_empty() {
            return Err(OrmError::Invalid(
                "a table needs at least one column".into(),
            ));
        }
        let qt = quote_ident(table)?;
        let mut defs = Vec::new();
        let mut fks = Vec::new();

        for c in &self.columns {
            let qn = quote_ident(&c.name)?;
            let mut d = format!("{qn} {}", c.ty);
            if c.primary {
                d.push_str(" PRIMARY KEY AUTOINCREMENT");
            } else {
                if !c.nullable {
                    d.push_str(" NOT NULL");
                }
                if c.unique {
                    d.push_str(" UNIQUE");
                }
                if let Some(v) = &c.default {
                    d.push_str(&format!(" DEFAULT {}", v.to_literal()));
                }
            }
            defs.push(d);

            if let Some((rt, rc)) = &c.references {
                fks.push(format!(
                    "FOREIGN KEY ({qn}) REFERENCES {}({}){}",
                    quote_ident(rt)?,
                    quote_ident(rc)?,
                    if c.on_delete_cascade {
                        " ON DELETE CASCADE"
                    } else {
                        ""
                    }
                ));
            }
        }
        defs.extend(fks);

        let mut out = vec![format!("CREATE TABLE {qt} ({})", defs.join(", "))];
        for (cols, unique) in &self.indexes {
            let quoted = cols
                .iter()
                .map(|c| quote_ident(c))
                .collect::<OrmResult<Vec<_>>>()?;
            let idx = quote_ident(&format!("idx_{}_{}", table, cols.join("_")))?;
            out.push(format!(
                "CREATE {}INDEX {idx} ON {qt} ({})",
                if *unique { "UNIQUE " } else { "" },
                quoted.join(", ")
            ));
        }
        Ok(out)
    }
}

pub struct Schema;

impl Schema {
    pub fn create(
        ex: &dyn Executor,
        table: &str,
        build: impl FnOnce(&mut Blueprint),
    ) -> OrmResult<()> {
        let mut bp = Blueprint::default();
        build(&mut bp);
        for stmt in bp.to_sql(table)? {
            ex.execute_batch(&stmt)?;
        }
        Ok(())
    }

    pub fn drop_if_exists(ex: &dyn Executor, table: &str) -> OrmResult<()> {
        ex.execute_batch(&format!("DROP TABLE IF EXISTS {}", quote_ident(table)?))
    }

    pub fn has_table(ex: &dyn Executor, table: &str) -> OrmResult<bool> {
        let rows = ex.query(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            &[SqlValue::Text(table.to_string())],
        )?;
        Ok(!rows.is_empty())
    }
}

// ----------------------------------------------------------------------
// migrations
// ----------------------------------------------------------------------

pub struct Migration {
    /// Unique and sortable, e.g. `20260101120000_create_posts`.
    pub name: &'static str,
    pub up: fn(&dyn Executor) -> OrmResult<()>,
    pub down: fn(&dyn Executor) -> OrmResult<()>,
}

/// Runs migrations in order and remembers what ran in a `_migrations`
/// table. Each migration runs in its own transaction.
#[derive(Default)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

impl Migrator {
    pub fn new(migrations: Vec<Migration>) -> Self {
        Migrator { migrations }
    }

    fn ensure_table(conn: &dyn Connection) -> OrmResult<()> {
        conn.as_executor().execute_batch(
            "CREATE TABLE IF NOT EXISTS \"_migrations\" (\
             \"name\" TEXT NOT NULL PRIMARY KEY, \
             \"batch\" INTEGER NOT NULL, \
             \"applied_at\" TEXT NOT NULL)",
        )
    }

    fn applied(conn: &dyn Connection) -> OrmResult<Vec<(String, i64)>> {
        let rows = conn.as_executor().query(
            "SELECT \"name\", \"batch\" FROM \"_migrations\" ORDER BY \"name\"",
            &[],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some((
                    r.get("name")?.as_str()?.to_string(),
                    r.get("batch")?.as_i64()?,
                ))
            })
            .collect())
    }

    /// Apply every pending migration as one batch. Returns their names.
    pub fn run(&self, conn: &dyn Connection) -> OrmResult<Vec<&'static str>> {
        Self::ensure_table(conn)?;
        let applied = Self::applied(conn)?;
        let batch = applied.iter().map(|(_, b)| *b).max().unwrap_or(0) + 1;

        let mut pending: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| !applied.iter().any(|(n, _)| n == m.name))
            .collect();
        pending.sort_by_key(|m| m.name);

        let mut ran = Vec::new();
        for m in pending {
            conn.transaction(&mut |tx| {
                (m.up)(tx)?;
                tx.execute(
                    "INSERT INTO \"_migrations\" (\"name\", \"batch\", \"applied_at\") VALUES (?, ?, ?)",
                    &[
                        SqlValue::Text(m.name.to_string()),
                        SqlValue::Int(batch),
                        SqlValue::Text(now_iso()),
                    ],
                )?;
                Ok(())
            })?;
            ran.push(m.name);
        }
        Ok(ran)
    }

    /// Undo the most recent batch, newest first. Returns their names.
    pub fn rollback(&self, conn: &dyn Connection) -> OrmResult<Vec<&'static str>> {
        Self::ensure_table(conn)?;
        let applied = Self::applied(conn)?;
        let Some(last) = applied.iter().map(|(_, b)| *b).max() else {
            return Ok(Vec::new());
        };

        let mut targets: Vec<&Migration> = self
            .migrations
            .iter()
            .filter(|m| applied.iter().any(|(n, b)| n == m.name && *b == last))
            .collect();
        targets.sort_by_key(|m| std::cmp::Reverse(m.name));

        let mut undone = Vec::new();
        for m in targets {
            conn.transaction(&mut |tx| {
                (m.down)(tx)?;
                tx.execute(
                    "DELETE FROM \"_migrations\" WHERE \"name\" = ?",
                    &[SqlValue::Text(m.name.to_string())],
                )?;
                Ok(())
            })?;
            undone.push(m.name);
        }
        Ok(undone)
    }

    /// `(name, applied?)` for every known migration.
    pub fn status(&self, conn: &dyn Connection) -> OrmResult<Vec<(&'static str, bool)>> {
        Self::ensure_table(conn)?;
        let applied = Self::applied(conn)?;
        let mut v: Vec<_> = self
            .migrations
            .iter()
            .map(|m| (m.name, applied.iter().any(|(n, _)| n == m.name)))
            .collect();
        v.sort_by_key(|(n, _)| *n);
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_sql() {
        let mut bp = Blueprint::default();
        bp.id();
        bp.string("title", 200).unique();
        bp.text("body").nullable();
        bp.boolean("published").default(false);
        bp.integer("user_id")
            .references("users", "id")
            .on_delete_cascade();
        bp.timestamps();
        bp.index(&["user_id"]);
        let sql = bp.to_sql("posts").unwrap();
        assert_eq!(
            sql[0],
            "CREATE TABLE \"posts\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT, \
             \"title\" VARCHAR(200) NOT NULL UNIQUE, \"body\" TEXT, \
             \"published\" BOOLEAN NOT NULL DEFAULT 0, \"user_id\" INTEGER NOT NULL, \
             \"created_at\" TEXT, \"updated_at\" TEXT, \
             FOREIGN KEY (\"user_id\") REFERENCES \"users\"(\"id\") ON DELETE CASCADE)"
        );
        assert_eq!(
            sql[1],
            "CREATE INDEX \"idx_posts_user_id\" ON \"posts\" (\"user_id\")"
        );
    }

    #[test]
    fn defaults_are_escaped_and_names_validated() {
        let mut bp = Blueprint::default();
        bp.string("note", 10).default("it's");
        assert!(bp.to_sql("t").unwrap()[0].contains("DEFAULT 'it''s'"));

        let mut bp = Blueprint::default();
        bp.string("a\"; DROP TABLE x; --", 10);
        assert!(bp.to_sql("t").is_err());
        assert!(Blueprint::default().to_sql("t").is_err());
        let mut bp = Blueprint::default();
        bp.id();
        assert!(bp.to_sql("t; DROP").is_err());
    }
}
