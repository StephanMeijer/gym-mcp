use std::time::Duration;

use sqlx::{PgPool, Postgres, Transaction};

/// Errors a tool can return. `User` messages are shown to the LLM so it can recover
/// (ask the user, pick a candidate, create the thing). `Db` is logged and hidden.
#[derive(Debug)]
pub enum AppError {
    User(String),
    Db(sqlx::Error),
}

pub type AppResult<T> = Result<T, AppError>;
pub type Tx = Transaction<'static, Postgres>;

pub fn user_err(msg: impl Into<String>) -> AppError {
    AppError::User(msg.into())
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            // SQLSTATE classes the caller can fix by changing its input.
            match db.code().as_deref() {
                Some("23505") => return user_err(format!("already exists ({})", db.constraint().unwrap_or("unique"))),
                Some("23514") => return user_err(format!("invalid value ({})", db.constraint().unwrap_or("check"))),
                Some("22007" | "22008") => return user_err(format!("invalid timestamp: {}", db.message())),
                Some("22023") => return user_err(format!("invalid parameter: {}", db.message())),
                _ => {}
            }
        }
        AppError::Db(e)
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::User(m) => f.write_str(m),
            AppError::Db(e) => write!(f, "database error: {e}"),
        }
    }
}

#[derive(Clone)]
pub struct App {
    pub pool: PgPool,
    pub default_timezone: String,
    pub stale_after: Duration,
}

/// Like `begin`, but the transaction is READ ONLY: Postgres rejects any write, which is what
/// backs the `read_only_hint` on read tools. Doesn't create the settings row.
pub async fn begin_read(app: &App, owner: &str) -> AppResult<Tx> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION READ ONLY").execute(&mut *tx).await?;
    let tz: String =
        sqlx::query_scalar("SELECT coalesce((SELECT timezone FROM user_settings WHERE owner_sub = $1), $2)")
            .bind(owner)
            .bind(&app.default_timezone)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query("SELECT set_config('TimeZone', $1, true)")
        .bind(&tz)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

/// Starts a transaction for `owner` with the session TimeZone set to the user's timezone.
/// From then on Postgres does all time handling: naive input strings cast with
/// `::timestamptz` are read as local time, `date_trunc`/`current_date` use local
/// calendar boundaries, and `to_json(timestamptz)` renders RFC 3339 with the local offset.
pub async fn begin(app: &App, owner: &str) -> AppResult<Tx> {
    let mut tx = app.pool.begin().await?;
    let tz: String = sqlx::query_scalar(
        "INSERT INTO user_settings (owner_sub, timezone) VALUES ($1, $2)
         ON CONFLICT (owner_sub) DO UPDATE SET timezone = user_settings.timezone
         RETURNING timezone",
    )
    .bind(owner)
    .bind(&app.default_timezone)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("SELECT set_config('TimeZone', $1, true)")
        .bind(&tz)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
