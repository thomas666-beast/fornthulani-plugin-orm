use fornthulani::ThulaniError;

#[derive(Debug, Clone)]
pub enum OrmError {
    /// A bad identifier, operator or argument. Almost always a programming
    /// error — or a client-controlled value (sort column, ...) that was not
    /// whitelisted.
    Invalid(String),
    /// The database failed. The message may contain SQL: it is logged, never
    /// sent to a client.
    Db(String),
    /// A UNIQUE / NOT NULL / FOREIGN KEY / CHECK constraint was violated.
    Constraint(String),
    NotFound(String),
    /// `Db::set` was never called.
    NoConnection,
}

pub type OrmResult<T> = Result<T, OrmError>;

impl std::fmt::Display for OrmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OrmError::Invalid(m) => write!(f, "invalid query: {m}"),
            OrmError::Db(m) => write!(f, "database error: {m}"),
            OrmError::Constraint(m) => write!(f, "constraint violation: {m}"),
            OrmError::NotFound(m) => write!(f, "not found: {m}"),
            OrmError::NoConnection => write!(f, "no database connection (call Db::set first)"),
        }
    }
}

impl std::error::Error for OrmError {}

/// So controllers can use `?` on ORM calls.
///
/// * `NotFound` -> 404
/// * `Constraint` -> 409 (generic text: constraint names reveal schema)
/// * `Invalid` -> 400 (generic text)
/// * everything else -> 500, details logged by the framework, not sent
impl From<OrmError> for ThulaniError {
    fn from(e: OrmError) -> Self {
        match e {
            OrmError::NotFound(m) => ThulaniError::NotFound(m),
            OrmError::Constraint(m) => {
                eprintln!("[fornthulani-orm] constraint violation: {m}");
                ThulaniError::Conflict("the request conflicts with existing data".into())
            }
            OrmError::Invalid(m) => {
                eprintln!("[fornthulani-orm] invalid query: {m}");
                ThulaniError::BadRequest("invalid query parameter".into())
            }
            other => ThulaniError::Internal(other.to_string()),
        }
    }
}
