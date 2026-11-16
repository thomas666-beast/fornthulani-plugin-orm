use serde_json::{Map, Value};

/// One database row: column name -> JSON value.
pub type Row = Map<String, Value>;

/// A value bound to a `?` placeholder. Values are *never* spliced into SQL
/// text; they always travel as bound parameters.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

impl SqlValue {
    pub fn from_json(v: &Value) -> SqlValue {
        match v {
            Value::Null => SqlValue::Null,
            Value::Bool(b) => SqlValue::Bool(*b),
            Value::Number(n) => match n.as_i64() {
                Some(i) => SqlValue::Int(i),
                None => match n.as_f64() {
                    Some(f) if f.is_finite() => SqlValue::Float(f),
                    _ => SqlValue::Text(n.to_string()),
                },
            },
            Value::String(s) => SqlValue::Text(s.clone()),
            // arrays / objects are stored as JSON text
            other => SqlValue::Text(other.to_string()),
        }
    }

    /// A SQL literal, for `DEFAULT` clauses in migrations only.
    pub(crate) fn to_literal(&self, d: crate::conn::Dialect) -> String {
        match self {
            SqlValue::Null => "NULL".into(),
            SqlValue::Bool(b) => d.bool_literal(*b).into(),
            SqlValue::Int(i) => i.to_string(),
            SqlValue::Float(f) if f.is_finite() => f.to_string(),
            SqlValue::Float(_) => "NULL".into(),
            SqlValue::Text(s) => format!("'{}'", s.replace('\'', "''")),
        }
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for SqlValue {
            fn from(v: $t) -> Self { SqlValue::Int(v as i64) }
        }
    )*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32, isize);

impl From<&SqlValue> for SqlValue {
    fn from(v: &SqlValue) -> Self {
        v.clone()
    }
}

impl From<bool> for SqlValue {
    fn from(v: bool) -> Self {
        SqlValue::Bool(v)
    }
}
impl From<f64> for SqlValue {
    fn from(v: f64) -> Self {
        SqlValue::Float(v)
    }
}
impl From<f32> for SqlValue {
    fn from(v: f32) -> Self {
        SqlValue::Float(v as f64)
    }
}
impl From<&str> for SqlValue {
    fn from(v: &str) -> Self {
        SqlValue::Text(v.to_string())
    }
}
impl From<String> for SqlValue {
    fn from(v: String) -> Self {
        SqlValue::Text(v)
    }
}
impl From<&String> for SqlValue {
    fn from(v: &String) -> Self {
        SqlValue::Text(v.clone())
    }
}
impl From<&Value> for SqlValue {
    fn from(v: &Value) -> Self {
        SqlValue::from_json(v)
    }
}
impl<T: Into<SqlValue>> From<Option<T>> for SqlValue {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => SqlValue::Null,
        }
    }
}

/// `\x<hex>`, PostgreSQL's text form of binary data.
pub(crate) fn hex_bytes(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("\\x");
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}
