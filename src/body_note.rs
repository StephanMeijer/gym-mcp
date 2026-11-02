//! Dated body notes: aches, pain and fatigue with a location and severity.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::measurement::measured_at;
use crate::workout::{now, workout_json};

/// The JSON of one `body_note` row.
const ROW: &str = "json_build_object('id', id, 'noted_at', noted_at, 'location', location, 'severity', severity,
                                     'notes', NULLIF(notes, ''), 'workout_id', workout_id)";

fn not_found(id: i64) -> db::AppError {
    user_err(format!("no body note with id {id}"))
}

/// Fails unless the workout exists and is the owner's.
async fn check_workout(tx: &mut Tx, owner: &str, id: Option<i64>) -> AppResult<()> {
    if let Some(id) = id {
        workout_json(tx, owner, id).await?;
    }
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
pub struct BodyNoteAddArgs {
    /// Where: a muscle group name when one fits ("lower_back", "quads"), otherwise free text ("left knee").
    pub location: String,
    /// How bad, 1 (barely noticeable) to 10 (can't train it).
    pub severity: Option<i16>,
    /// When, if not now. RFC 3339, or local time like "2026-11-02 21:00".
    pub noted_at: Option<String>,
    pub notes: Option<String>,
    /// The workout it came up in, if any.
    pub workout_id: Option<i64>,
}

pub async fn body_note_add(app: &App, owner: &str, a: BodyNoteAddArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    check_workout(&mut tx, owner, a.workout_id).await?;
    let at = match measured_at(&mut tx, &a.noted_at).await? {
        Some(at) => at,
        None => now(&mut tx).await?,
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "INSERT INTO body_note (owner_sub, noted_at, location, severity, notes, workout_id)
         VALUES ($1, $2, lower(btrim($3)), $4, coalesce($5, ''), $6) RETURNING {ROW}"
    )))
    .bind(owner)
    .bind(at)
    .bind(&a.location)
    .bind(a.severity)
    .bind(&a.notes)
    .bind(a.workout_id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct BodyNoteListArgs {
    /// Only notes whose location contains this ("back").
    pub location: Option<String>,
    /// From (inclusive). RFC 3339 or local time/date like "2026-10-01".
    pub from: Option<String>,
    /// To (exclusive).
    pub to: Option<String>,
}

pub async fn body_note_list(app: &App, owner: &str, a: BodyNoteListArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg({ROW} ORDER BY noted_at DESC), '[]'::json) FROM body_note
         WHERE owner_sub = $1
           AND ($2::text IS NULL OR location ILIKE '%' || btrim($2) || '%')
           AND noted_at >= coalesce($3::timestamptz, '-infinity')
           AND noted_at < coalesce($4::timestamptz, 'infinity')"
    )))
    .bind(owner)
    .bind(&a.location)
    .bind(&a.from)
    .bind(&a.to)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct BodyNoteUpdateArgs {
    pub id: i64,
    pub location: Option<String>,
    pub severity: Option<i16>,
    pub noted_at: Option<String>,
    pub notes: Option<String>,
    pub workout_id: Option<i64>,
}

pub async fn body_note_update(app: &App, owner: &str, a: BodyNoteUpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    check_workout(&mut tx, owner, a.workout_id).await?;
    let at = measured_at(&mut tx, &a.noted_at).await?;
    let v: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "UPDATE body_note SET location = coalesce(lower(btrim($3)), location), severity = coalesce($4, severity),
                              noted_at = coalesce($5, noted_at), notes = coalesce($6, notes),
                              workout_id = coalesce($7, workout_id)
         WHERE id = $1 AND owner_sub = $2 RETURNING {ROW}"
    )))
    .bind(a.id)
    .bind(owner)
    .bind(&a.location)
    .bind(a.severity)
    .bind(at)
    .bind(&a.notes)
    .bind(a.workout_id)
    .fetch_optional(&mut *tx)
    .await?;
    let v = v.ok_or_else(|| not_found(a.id))?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct BodyNoteDeleteArgs {
    pub id: i64,
}

pub async fn body_note_delete(app: &App, owner: &str, a: BodyNoteDeleteArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let v: Option<Value> = sqlx::query_scalar(AssertSqlSafe(format!(
        "DELETE FROM body_note WHERE id = $1 AND owner_sub = $2 RETURNING {ROW}"
    )))
    .bind(a.id)
    .bind(owner)
    .fetch_optional(&mut *tx)
    .await?;
    let v = v.ok_or_else(|| not_found(a.id))?;
    tx.commit().await?;
    Ok(json!({"deleted": v}))
}

/// Body notes from the last 14 days, newest first. Used by training_overview.
pub async fn recent_json(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    Ok(sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg({ROW} ORDER BY noted_at DESC), '[]'::json) FROM body_note
         WHERE owner_sub = $1 AND noted_at >= now() - interval '14 days'"
    )))
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?)
}
