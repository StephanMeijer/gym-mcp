//! Workout lifecycle (start / resume / supersede / stale closure) and set logging.

use chrono::{DateTime, Duration, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::resolve::{self, Kind};

const MAX_FUTURE: Duration = Duration::minutes(5);

/// The open workout's facts that every lifecycle rule needs.
struct Open {
    id: i64,
    gym_id: i64,
    gym: String,
    last_activity: DateTime<Utc>,
    set_count: i64,
    has_notes: bool,
}

async fn open_workout(tx: &mut Tx, owner: &str) -> AppResult<Option<Open>> {
    let row: Option<(i64, i64, String, DateTime<Utc>, i64, bool)> = sqlx::query_as(
        "SELECT w.id, w.gym_id, g.name,
                coalesce(max(s.performed_at), w.started_at), count(s.id), w.notes <> ''
         FROM workout w JOIN gym g ON g.id = w.gym_id
         LEFT JOIN workout_set s ON s.workout_id = w.id
         WHERE w.owner_sub = $1 AND w.finished_at IS NULL
         GROUP BY w.id, g.name",
    )
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(|(id, gym_id, gym, last_activity, set_count, has_notes)| Open {
        id,
        gym_id,
        gym,
        last_activity,
        set_count,
        has_notes,
    }))
}

#[derive(Debug, PartialEq)]
pub enum StaleAction {
    /// Nothing was recorded: an accidental start.
    Delete,
    /// Finish at the last activity (last set, or the start if there are only notes).
    Finish,
}

pub fn stale_action(set_count: i64, has_notes: bool) -> StaleAction {
    if set_count == 0 && !has_notes {
        StaleAction::Delete
    } else {
        StaleAction::Finish
    }
}

/// Closes the open workout if its last activity is older than `stale_after`.
/// Returns a description of what was done, for the tool result. With `save = false`
/// (read-only tools) it only reports what the next workout action will do; the outcome is
/// deterministic, so the preview and the later closure are identical.
pub async fn close_stale(app: &App, tx: &mut Tx, owner: &str, save: bool) -> AppResult<Option<Value>> {
    let Some(open) = open_workout(tx, owner).await? else {
        return Ok(None);
    };
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT now()").fetch_one(&mut **tx).await?;
    let stale_after = Duration::from_std(app.stale_after).unwrap_or(Duration::hours(3));
    if open.last_activity >= now - stale_after {
        return Ok(None);
    }
    let action = stale_action(open.set_count, open.has_notes);
    match action {
        _ if !save => {}
        StaleAction::Delete => {
            sqlx::query("DELETE FROM workout WHERE id = $1")
                .bind(open.id)
                .execute(&mut **tx)
                .await?;
        }
        StaleAction::Finish => {
            sqlx::query("UPDATE workout SET finished_at = $2, finish_reason = 'stale' WHERE id = $1")
                .bind(open.id)
                .bind(open.last_activity)
                .execute(&mut **tx)
                .await?;
        }
    }
    let at: Value = sqlx::query_scalar("SELECT to_json($1::timestamptz)")
        .bind(open.last_activity)
        .fetch_one(&mut **tx)
        .await?;
    Ok(Some(json!({
        "workout_id": open.id,
        "gym": open.gym,
        "action": match (action, save) {
            (StaleAction::Delete, true) => "deleted (no sets, no notes)",
            (StaleAction::Finish, true) => "auto-finished (stale)",
            (StaleAction::Delete, false) => "stale: will be deleted on the next workout action (no sets, no notes)",
            (StaleAction::Finish, false) => "stale: will be auto-finished on the next workout action",
        },
        "last_activity": at,
        "sets": open.set_count,
    })))
}

/// The full view of one workout: sets grouped by exercise, numbered by performed_at.
pub async fn workout_json(tx: &mut Tx, owner: &str, workout_id: i64) -> AppResult<Value> {
    let v: Option<Value> = sqlx::query_scalar(
        "SELECT json_build_object(
            'workout_id', w.id,
            'gym', json_build_object('id', g.id, 'name', g.name),
            'started_at', w.started_at,
            'finished_at', w.finished_at,
            'finish_reason', w.finish_reason,
            'notes', NULLIF(w.notes, ''),
            'set_count', (SELECT count(*) FROM workout_set WHERE workout_id = w.id),
            'exercises', coalesce((
                SELECT json_agg(x.obj ORDER BY x.first_at)
                FROM (
                    SELECT min(s.performed_at) AS first_at,
                           json_build_object(
                               'exercise_id', e.id,
                               'exercise', e.name,
                               'sets', json_agg(json_build_object(
                                   'set_id', s.id, 'set_number', s.n, 'performed_at', s.performed_at,
                                   'reps', s.reps, 'weight_kg', s.weight_kg, 'duration_s', s.duration_s,
                                   'rpe', s.rpe, 'rest_s', s.rest_s, 'set_type', s.set_type,
                                   'unilateral', s.unilateral, 'load_basis', s.load_basis,
                                   'distance_m', s.distance_m, 'intensity', NULLIF(s.intensity, ''),
                                   'machine', s.machine, 'variant', s.variant,
                                   'notes', NULLIF(s.notes, '')) ORDER BY s.performed_at, s.id)) AS obj
                    FROM (SELECT ws.*, row_number() OVER (PARTITION BY ws.exercise_id ORDER BY ws.performed_at, ws.id) AS n
                          FROM workout_set ws WHERE ws.workout_id = w.id) s
                    JOIN exercise e ON e.id = s.exercise_id
                    GROUP BY e.id, e.name
                ) x), '[]'::json))
         FROM workout w JOIN gym g ON g.id = w.gym_id
         WHERE w.id = $1 AND w.owner_sub = $2",
    )
    .bind(workout_id)
    .bind(owner)
    .fetch_optional(&mut **tx)
    .await?;
    v.ok_or_else(|| user_err(format!("no workout with id {workout_id}")))
}

pub(crate) async fn parse_time(tx: &mut Tx, s: &str) -> AppResult<DateTime<Utc>> {
    Ok(sqlx::query_scalar("SELECT $1::timestamptz")
        .bind(s)
        .fetch_one(&mut **tx)
        .await?)
}

pub(crate) async fn now(tx: &mut Tx) -> AppResult<DateTime<Utc>> {
    Ok(sqlx::query_scalar("SELECT now()").fetch_one(&mut **tx).await?)
}

pub(crate) fn not_future(at: DateTime<Utc>, now: DateTime<Utc>) -> AppResult<()> {
    if at > now + MAX_FUTURE {
        return Err(user_err(
            "that time is in the future; pass the time the set was actually performed",
        ));
    }
    Ok(())
}

// ---------- workout tools ----------

#[derive(Deserialize, JsonSchema)]
pub struct StartArgs {
    /// Gym name, alias, or id ("ndsm trainmore").
    pub gym: String,
    /// When you arrived, if not now. RFC 3339, or local time like "2026-11-02 18:05".
    pub started_at: Option<String>,
    /// Set true to finish a workout that is still open at a *different* gym and start this one.
    #[serde(default)]
    pub finish_open: bool,
}

pub async fn start(app: &App, owner: &str, a: StartArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let closed = close_stale(app, &mut tx, owner, true).await?;
    let gym_id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    let mut superseded = None;

    if let Some(open) = open_workout(&mut tx, owner).await? {
        if open.gym_id == gym_id {
            let w = workout_json(&mut tx, owner, open.id).await?;
            tx.commit().await?;
            return Ok(json!({"status": "resumed", "workout": w, "closed_stale": closed}));
        }
        if !a.finish_open {
            let w = workout_json(&mut tx, owner, open.id).await?;
            return Err(user_err(
                json!({
                    "error": format!("a workout is still open at {} ({} sets)", open.gym, open.set_count),
                    "open_workout": w,
                    "hint": "ask the user; to finish it and start here, call workout_start again with finish_open: true",
                })
                .to_string(),
            ));
        }
        sqlx::query("UPDATE workout SET finished_at = $2, finish_reason = 'superseded' WHERE id = $1")
            .bind(open.id)
            .bind(open.last_activity)
            .execute(&mut *tx)
            .await?;
        superseded = Some(json!({"workout_id": open.id, "gym": open.gym}));
    }

    let now = now(&mut tx).await?;
    let started_at = match &a.started_at {
        Some(s) => parse_time(&mut tx, s).await?,
        None => now,
    };
    not_future(started_at, now)?;
    let id: i64 =
        sqlx::query_scalar("INSERT INTO workout (owner_sub, gym_id, started_at) VALUES ($1, $2, $3) RETURNING id")
            .bind(owner)
            .bind(gym_id)
            .bind(started_at)
            .fetch_one(&mut *tx)
            .await?;
    let w = workout_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(json!({"status": "started", "workout": w, "superseded": superseded, "closed_stale": closed}))
}

#[derive(Deserialize, JsonSchema)]
pub struct AddPastArgs {
    pub gym: String,
    /// RFC 3339, or local time like "2026-11-01 18:00".
    pub started_at: String,
    pub finished_at: String,
    pub notes: Option<String>,
}

/// A workout that already happened ("forgot to log yesterday"). Created finished, so it never
/// touches the open workout and is never stale; add sets with set_add(workout_id, performed_at).
pub async fn add_past(app: &App, owner: &str, a: AddPastArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let gym_id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    let (start, end) = (
        parse_time(&mut tx, &a.started_at).await?,
        parse_time(&mut tx, &a.finished_at).await?,
    );
    let now = now(&mut tx).await?;
    not_future(end, now)?;
    if end < start {
        return Err(user_err("finished_at is before started_at"));
    }
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO workout (owner_sub, gym_id, started_at, finished_at, finish_reason, notes)
         VALUES ($1, $2, $3, $4, 'manual', coalesce($5, '')) RETURNING id",
    )
    .bind(owner)
    .bind(gym_id)
    .bind(start)
    .bind(end)
    .bind(&a.notes)
    .fetch_one(&mut *tx)
    .await?;
    let w = workout_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(w)
}

#[derive(Deserialize, JsonSchema)]
pub struct FinishArgs {
    /// When you left, if not now. Must be at or after the last set.
    pub finished_at: Option<String>,
}

pub async fn finish(app: &App, owner: &str, a: FinishArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    if let Some(closed) = close_stale(app, &mut tx, owner, true).await? {
        tx.commit().await?;
        return Ok(json!({"status": "already auto-finished because it was stale", "closed_stale": closed}));
    }
    let open = open_workout(&mut tx, owner)
        .await?
        .ok_or_else(|| user_err("no open workout"))?;
    let now = now(&mut tx).await?;
    let at = match &a.finished_at {
        Some(s) => parse_time(&mut tx, s).await?,
        None => now,
    };
    not_future(at, now)?;
    if at < open.last_activity {
        return Err(user_err("finished_at is before the last logged set"));
    }
    sqlx::query("UPDATE workout SET finished_at = $2, finish_reason = 'manual' WHERE id = $1")
        .bind(open.id)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    let w = workout_json(&mut tx, owner, open.id).await?;
    tx.commit().await?;
    Ok(json!({"status": "finished", "workout": w}))
}

#[derive(Deserialize, JsonSchema)]
pub struct UpdateArgs {
    pub workout_id: i64,
    pub started_at: Option<String>,
    /// Setting this on the open workout finishes it.
    pub finished_at: Option<String>,
    /// Move the workout to another gym (name, alias, or id).
    pub gym: Option<String>,
    pub notes: Option<String>,
}

pub async fn update(app: &App, owner: &str, a: UpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    workout_json(&mut tx, owner, a.workout_id).await?; // ownership check
    let gym_id = match &a.gym {
        Some(g) => Some(resolve::id(&mut tx, owner, Kind::Gym, g).await?),
        None => None,
    };
    let started = match &a.started_at {
        Some(s) => Some(parse_time(&mut tx, s).await?),
        None => None,
    };
    let finished = match &a.finished_at {
        Some(s) => Some(parse_time(&mut tx, s).await?),
        None => None,
    };
    let now = now(&mut tx).await?;
    for t in started.iter().chain(&finished) {
        not_future(*t, now)?;
    }
    sqlx::query(
        "UPDATE workout SET
            started_at = coalesce($2, started_at),
            finished_at = coalesce($3, finished_at),
            finish_reason = CASE WHEN $3::timestamptz IS NOT NULL AND finished_at IS NULL THEN 'manual' ELSE finish_reason END,
            gym_id = coalesce($4, gym_id),
            notes = coalesce($5, notes)
         WHERE id = $1",
    )
    .bind(a.workout_id)
    .bind(started)
    .bind(finished)
    .bind(gym_id)
    .bind(&a.notes)
    .execute(&mut *tx)
    .await?;
    let outside: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM workout_set s JOIN workout w ON w.id = s.workout_id
                        WHERE w.id = $1 AND (s.performed_at < w.started_at OR s.performed_at > w.finished_at))",
    )
    .bind(a.workout_id)
    .fetch_one(&mut *tx)
    .await?;
    if outside {
        return Err(user_err(
            "some sets would fall outside the workout's start/finish; adjust those sets first",
        ));
    }
    let w = workout_json(&mut tx, owner, a.workout_id).await?;
    tx.commit().await?;
    Ok(w)
}

pub async fn current(app: &App, owner: &str) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let closed = close_stale(app, &mut tx, owner, false).await?;
    let w = match open_workout(&mut tx, owner).await? {
        Some(o) if closed.is_none() => workout_json(&mut tx, owner, o.id).await?,
        _ => Value::Null,
    };
    Ok(json!({"open_workout": w, "closed_stale": closed}))
}

// ---------- sets ----------

/// Only working sets count towards volume, progress, PRs and last_session.
#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum SetType {
    /// Warm-up, e.g. an empty bar before the working sets.
    Warmup,
    #[default]
    Working,
    /// Deliberately partial range of motion.
    Partial,
    /// Stopped early (cramp, pain, equipment, phone call).
    Interrupted,
}

impl SetType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SetType::Warmup => "warmup",
            SetType::Working => "working",
            SetType::Partial => "partial",
            SetType::Interrupted => "interrupted",
        }
    }
}

/// What weight_kg means on a unilateral set. Omit for the default convention.
#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum LoadBasis {
    /// The default convention; in set_update, clears an earlier load_basis.
    Default,
    /// The number on one dumbbell/kettlebell.
    PerImplement,
    /// The load one side moved.
    PerSide,
    /// The load for both sides together.
    Total,
}

impl LoadBasis {
    /// "default" is stored as NULL.
    fn as_str(self) -> &'static str {
        match self {
            LoadBasis::Default => "default",
            LoadBasis::PerImplement => "per_implement",
            LoadBasis::PerSide => "per_side",
            LoadBasis::Total => "total",
        }
    }

    fn is_set(b: Option<Self>) -> bool {
        b.is_some_and(|b| !matches!(b, LoadBasis::Default))
    }
}

const LOAD_BASIS_NEEDS_UNILATERAL: &str = "load_basis applies only to unilateral sets; pass unilateral true";

#[derive(Deserialize, JsonSchema)]
pub struct SetInput {
    pub reps: Option<i32>,
    /// The number on the implement: per dumbbell/kettlebell; barbell = total incl. bar; machine = stack setting.
    pub weight_kg: Option<f64>,
    pub duration_s: Option<i32>,
    /// Cardio: distance in metres ("7.5 km" = 7500).
    pub distance_m: Option<f64>,
    /// Cardio: machine setting as free text, e.g. "level 12, incline 6%".
    pub intensity: Option<String>,
    /// Effort 1-10 in half steps. Reps in reserve map as RPE = 10 - RIR ("2 left in the tank" = 8).
    pub rpe: Option<f64>,
    /// Rest before this set, in seconds.
    pub rest_s: Option<i32>,
    /// Default working. Warm-ups and sets cut short don't count towards volume, progress or PRs.
    pub set_type: Option<SetType>,
    /// When the set was performed, if not just now. RFC 3339, or local time like "19:40" on a date: "2026-11-02 19:40".
    pub performed_at: Option<String>,
    pub notes: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SetAddArgs {
    /// Exercise name, alias, or id.
    pub exercise: String,
    /// One entry per set, in the order performed. "10/8/6 on 20kg" = three entries.
    pub sets: Vec<SetInput>,
    /// Only for logging into a past (finished) workout; then every set needs performed_at. Default: the open workout.
    pub workout_id: Option<i64>,
    /// Machine brand/model for all these sets ("Technogym Selection"), read off the photo when visible.
    /// Omit to reuse the last machine for this exercise at this gym; "" = unknown.
    pub machine: Option<String>,
    /// Grip or variant for all these sets ("neutral grip", "wide", "seated").
    /// Omit to reuse the last variant for this exercise at this gym; "" = standard.
    pub variant: Option<String>,
    /// One arm/leg at a time: reps are per side and each set counts once per side. Default false.
    pub unilateral: Option<bool>,
    /// For all these sets; omit for the default convention.
    pub load_basis: Option<LoadBasis>,
}

/// The machine or variant (`column`) for new sets and where it came from: given, last_used_here, or none.
async fn last_here(
    tx: &mut Tx,
    owner: &str,
    workout_id: i64,
    exercise_id: i64,
    column: &'static str,
    given: &Option<String>,
) -> AppResult<(Option<String>, &'static str)> {
    if let Some(m) = given {
        let m = m.trim();
        return Ok(if m.is_empty() {
            (None, "none")
        } else {
            (Some(m.to_owned()), "given")
        });
    }
    // column is a compile-time literal ("machine" or "variant"); values are bound.
    let last: Option<String> = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT s.{column} FROM workout_set s JOIN workout w ON w.id = s.workout_id
         WHERE w.owner_sub = $1 AND s.exercise_id = $3 AND s.{column} IS NOT NULL
           AND w.gym_id = (SELECT gym_id FROM workout WHERE id = $2)
         ORDER BY s.performed_at DESC LIMIT 1"
    )))
    .bind(owner)
    .bind(workout_id)
    .bind(exercise_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(match last {
        Some(m) => (Some(m), "last_used_here"),
        None => (None, "none"),
    })
}

struct Target {
    id: i64,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

async fn target(tx: &mut Tx, owner: &str, workout_id: i64) -> AppResult<Target> {
    let row: Option<(DateTime<Utc>, Option<DateTime<Utc>>)> =
        sqlx::query_as("SELECT started_at, finished_at FROM workout WHERE id = $1 AND owner_sub = $2")
            .bind(workout_id)
            .bind(owner)
            .fetch_optional(&mut **tx)
            .await?;
    let (started_at, finished_at) = row.ok_or_else(|| user_err(format!("no workout with id {workout_id}")))?;
    Ok(Target {
        id: workout_id,
        started_at,
        finished_at,
    })
}

/// Checks `at` against the workout window. For an open workout an earlier set moves the start back.
async fn fit_in_window(tx: &mut Tx, t: &mut Target, at: DateTime<Utc>, now: DateTime<Utc>) -> AppResult<()> {
    not_future(at, now)?;
    if let Some(fin) = t.finished_at {
        if at < t.started_at || at > fin {
            return Err(user_err(
                "performed_at is outside that workout's start/finish; use workout_update to widen it, or pick another workout",
            ));
        }
    } else if at < t.started_at {
        sqlx::query("UPDATE workout SET started_at = $2 WHERE id = $1")
            .bind(t.id)
            .bind(at)
            .execute(&mut **tx)
            .await?;
        t.started_at = at;
    }
    Ok(())
}

pub async fn set_add(app: &App, owner: &str, a: SetAddArgs) -> AppResult<Value> {
    if a.sets.is_empty() || a.sets.len() > 50 {
        return Err(user_err("pass between 1 and 50 sets"));
    }
    if a.sets
        .iter()
        .any(|s| s.reps.is_none() && s.duration_s.is_none() && s.distance_m.is_none())
    {
        return Err(user_err("each set needs reps, duration_s or distance_m"));
    }
    if LoadBasis::is_set(a.load_basis) && a.unilateral != Some(true) {
        return Err(user_err(LOAD_BASIS_NEEDS_UNILATERAL));
    }
    let mut tx = db::begin(app, owner).await?;
    let mut closed = None;
    let workout_id = match a.workout_id {
        Some(id) => id,
        None => {
            closed = close_stale(app, &mut tx, owner, true).await?;
            match open_workout(&mut tx, owner).await? {
                Some(o) => o.id,
                None => {
                    tx.commit().await?; // keep the stale closure
                    return Err(user_err(
                        json!({"error": "no open workout; call workout_start first (or pass workout_id for a past workout)",
                               "closed_stale": closed})
                        .to_string(),
                    ));
                }
            }
        }
    };
    let mut t = target(&mut tx, owner, workout_id).await?;
    let exercise_id = resolve::id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let now = now(&mut tx).await?;
    let original_start = t.started_at;
    let (machine, machine_source) = last_here(&mut tx, owner, t.id, exercise_id, "machine", &a.machine).await?;
    let (variant, variant_source) = last_here(&mut tx, owner, t.id, exercise_id, "variant", &a.variant).await?;

    for (i, s) in a.sets.iter().enumerate() {
        let at = match &s.performed_at {
            Some(p) => parse_time(&mut tx, p).await?,
            None if t.finished_at.is_some() => {
                return Err(user_err("that workout is finished; every set needs performed_at"));
            }
            // Keep the given order for sets logged together without times.
            None => now + Duration::milliseconds(i as i64),
        };
        fit_in_window(&mut tx, &mut t, at, now).await?;
        sqlx::query(
            "INSERT INTO workout_set (workout_id, exercise_id, performed_at, reps, weight_kg, duration_s, rpe, rest_s, notes,
                                      distance_m, intensity, machine, set_type, unilateral, load_basis, variant)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, coalesce($9, ''), $10, coalesce($11, ''), $12, $13, $14, NULLIF($15, 'default'), $16)",
        )
        .bind(t.id)
        .bind(exercise_id)
        .bind(at)
        .bind(s.reps)
        .bind(s.weight_kg)
        .bind(s.duration_s)
        .bind(s.rpe)
        .bind(s.rest_s)
        .bind(&s.notes)
        .bind(s.distance_m)
        .bind(&s.intensity)
        .bind(&machine)
        .bind(s.set_type.unwrap_or_default().as_str())
        .bind(a.unilateral.unwrap_or(false))
        .bind(a.load_basis.map(LoadBasis::as_str))
        .bind(&variant)
        .execute(&mut *tx)
        .await?;
    }
    let w = workout_json(&mut tx, owner, t.id).await?;
    tx.commit().await?;
    Ok(json!({
        "added": a.sets.len(),
        "machine": machine,
        "machine_source": machine_source,
        "variant": variant,
        "variant_source": variant_source,
        "workout": w,
        "started_at_moved_back": t.started_at != original_start,
        "closed_stale": closed,
    }))
}

#[derive(Deserialize, JsonSchema)]
pub struct SetUpdateArgs {
    pub set_id: i64,
    /// Move the set to another exercise (name, alias, or id).
    pub exercise: Option<String>,
    pub reps: Option<i32>,
    pub weight_kg: Option<f64>,
    pub duration_s: Option<i32>,
    pub distance_m: Option<f64>,
    pub intensity: Option<String>,
    /// "" = unknown.
    pub machine: Option<String>,
    /// Grip or variant; "" = standard.
    pub variant: Option<String>,
    /// 1-10 in half steps.
    pub rpe: Option<f64>,
    pub rest_s: Option<i32>,
    pub set_type: Option<SetType>,
    /// Reps are per side; the set counts once per side. false also clears load_basis.
    pub unilateral: Option<bool>,
    /// Only on unilateral sets; default = back to the default convention.
    pub load_basis: Option<LoadBasis>,
    pub performed_at: Option<String>,
    pub notes: Option<String>,
}

async fn set_workout(tx: &mut Tx, owner: &str, set_id: i64) -> AppResult<i64> {
    sqlx::query_scalar("SELECT s.workout_id FROM workout_set s JOIN workout w ON w.id = s.workout_id WHERE s.id = $1 AND w.owner_sub = $2")
        .bind(set_id)
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| user_err(format!("no set with id {set_id}")))
}

pub async fn set_update(app: &App, owner: &str, a: SetUpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let workout_id = set_workout(&mut tx, owner, a.set_id).await?;
    if LoadBasis::is_set(a.load_basis) {
        let unilateral = match a.unilateral {
            Some(u) => u,
            None => {
                sqlx::query_scalar("SELECT unilateral FROM workout_set WHERE id = $1")
                    .bind(a.set_id)
                    .fetch_one(&mut *tx)
                    .await?
            }
        };
        if !unilateral {
            return Err(user_err(LOAD_BASIS_NEEDS_UNILATERAL));
        }
    }
    let exercise_id = match &a.exercise {
        Some(e) => Some(resolve::id(&mut tx, owner, Kind::Exercise, e).await?),
        None => None,
    };
    let at = match &a.performed_at {
        Some(p) => {
            let at = parse_time(&mut tx, p).await?;
            let now = now(&mut tx).await?;
            let mut t = target(&mut tx, owner, workout_id).await?;
            fit_in_window(&mut tx, &mut t, at, now).await?;
            Some(at)
        }
        None => None,
    };
    sqlx::query(
        "UPDATE workout_set SET
            exercise_id = coalesce($2, exercise_id), reps = coalesce($3, reps), weight_kg = coalesce($4, weight_kg),
            duration_s = coalesce($5, duration_s), performed_at = coalesce($6, performed_at),
            notes = coalesce($7, notes), rpe = coalesce($8, rpe), rest_s = coalesce($9, rest_s),
            distance_m = coalesce($10, distance_m), intensity = coalesce($11, intensity),
            machine = CASE WHEN $12::text IS NULL THEN machine ELSE NULLIF(btrim($12), '') END,
            set_type = coalesce($13, set_type), unilateral = coalesce($14, unilateral),
            load_basis = CASE WHEN NOT coalesce($14, unilateral) THEN NULL WHEN $15::text IS NULL THEN load_basis ELSE NULLIF($15, 'default') END,
            variant = CASE WHEN $16::text IS NULL THEN variant ELSE NULLIF(btrim($16), '') END, updated_at = now()
         WHERE id = $1",
    )
    .bind(a.set_id)
    .bind(exercise_id)
    .bind(a.reps)
    .bind(a.weight_kg)
    .bind(a.duration_s)
    .bind(at)
    .bind(&a.notes)
    .bind(a.rpe)
    .bind(a.rest_s)
    .bind(a.distance_m)
    .bind(&a.intensity)
    .bind(&a.machine)
    .bind(a.set_type.map(SetType::as_str))
    .bind(a.unilateral)
    .bind(a.load_basis.map(LoadBasis::as_str))
    .bind(&a.variant)
    .execute(&mut *tx)
    .await?;
    let w = workout_json(&mut tx, owner, workout_id).await?;
    tx.commit().await?;
    Ok(w)
}

#[derive(Deserialize, JsonSchema)]
pub struct SetDeleteArgs {
    pub set_id: i64,
}

pub async fn set_delete(app: &App, owner: &str, a: SetDeleteArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let workout_id = set_workout(&mut tx, owner, a.set_id).await?;
    sqlx::query("DELETE FROM workout_set WHERE id = $1")
        .bind(a.set_id)
        .execute(&mut *tx)
        .await?;
    let w = workout_json(&mut tx, owner, workout_id).await?;
    tx.commit().await?;
    Ok(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_rules() {
        assert_eq!(stale_action(0, false), StaleAction::Delete);
        assert_eq!(stale_action(0, true), StaleAction::Finish);
        assert_eq!(stale_action(3, false), StaleAction::Finish);
    }
}
