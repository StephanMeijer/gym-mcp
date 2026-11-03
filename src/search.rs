//! Read-only search and analysis. All dates are by performed_at, in the user's timezone
//! (the transaction's TimeZone is set by `db::begin`).

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::Deserialize;
// Dynamic SQL only interpolates compile-time constants and enum-mapped literals; user input is always bound.
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use sqlx::{Postgres, QueryBuilder};

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::resolve::{self, Kind};
use crate::setup::{self, MUSCLE_GROUPS, expand_opt};
use crate::workout;

// Macros so the consts below can be built with concat! and stay const.
// The load one side moved: a unilateral set logged as a total is split over both sides.
macro_rules! load {
    () => {
        "(s.weight_kg / CASE WHEN s.unilateral AND s.load_basis = 'total' THEN 2 ELSE 1 END)"
    };
}
// A unilateral set counts once per side (reps are per side).
macro_rules! sides {
    () => {
        "(CASE WHEN s.unilateral THEN 2 ELSE 1 END)"
    };
}
const LOAD: &str = load!();
const SIDES: &str = sides!();
const E1RM: &str = concat!("(", load!(), " * (1 + s.reps / 30.0))"); // Epley; per limb for unilateral sets
const VOLUME: &str = concat!("(", load!(), " * s.reps * ", sides!(), ")");
// Groups history per machine and variant; '' = unknown machine / standard variant.
const SETUP_KEY: &str = "(coalesce(lower(s.machine), '') || '|' || coalesce(lower(s.variant), ''))";
const PACE: &str = "(s.duration_s * 1000.0 / NULLIF(s.distance_m, 0))"; // seconds per km

fn limit(l: Option<i64>) -> i64 {
    l.unwrap_or(50).clamp(1, 500)
}

async fn opt_id(tx: &mut Tx, owner: &str, kind: Kind, q: &Option<String>) -> AppResult<Option<i64>> {
    match q {
        Some(q) => Ok(Some(resolve::id(tx, owner, kind, q).await?)),
        None => Ok(None),
    }
}

/// The gym asked for, else the open workout's gym.
async fn gym_or_current(tx: &mut Tx, owner: &str, gym: &Option<String>) -> AppResult<Option<i64>> {
    if gym.is_some() {
        return opt_id(tx, owner, Kind::Gym, gym).await;
    }
    Ok(
        sqlx::query_scalar("SELECT gym_id FROM workout WHERE owner_sub = $1 AND finished_at IS NULL")
            .bind(owner)
            .fetch_optional(&mut **tx)
            .await?,
    )
}

// ---------- training_overview ----------

#[derive(Deserialize, JsonSchema)]
pub struct OverviewArgs {
    /// Gym name, alias, or id. Default: the gym of the open workout.
    pub gym: Option<String>,
}

pub async fn training_overview(app: &App, owner: &str, a: OverviewArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let closed = workout::close_stale(app, &mut tx, owner, false).await?;
    let gym_id = gym_or_current(&mut tx, owner, &a.gym)
        .await?
        .ok_or_else(|| user_err("no open workout; pass a gym"))?;
    let mut v: Value = sqlx::query_scalar(
        "WITH mine AS (SELECT s.*, w.gym_id FROM workout_set s JOIN workout w ON w.id = s.workout_id WHERE w.owner_sub = $1),
         -- warm-ups and cut-short sets only count for machine memory
         work AS (SELECT * FROM mine WHERE set_type = 'working'),
         stats AS (SELECT exercise_id, max(performed_at) AS last_done,
                          count(*) FILTER (WHERE performed_at >= now() - interval '30 days') AS sets_30d
                   FROM work GROUP BY exercise_id),
         -- the machine and the variant last used for each exercise at this gym
         here AS (SELECT exercise_id,
                         (array_agg(machine ORDER BY performed_at DESC) FILTER (WHERE machine IS NOT NULL))[1] AS machine,
                         (array_agg(variant ORDER BY performed_at DESC))[1] AS variant
                  FROM mine WHERE gym_id = $2 GROUP BY exercise_id),
         -- the last session on that machine and variant at any gym (same brand compares across gyms), else the last session
         picked AS (
            SELECT s.* FROM work s LEFT JOIN here h ON h.exercise_id = s.exercise_id
            WHERE (h.machine IS NULL OR lower(s.machine) = lower(h.machine))
              AND (h.exercise_id IS NULL OR lower(s.variant) IS NOT DISTINCT FROM lower(h.variant))
              AND s.workout_id = (SELECT m.workout_id FROM work m
                                  WHERE m.exercise_id = s.exercise_id AND (h.machine IS NULL OR lower(m.machine) = lower(h.machine))
                                    AND (h.exercise_id IS NULL OR lower(m.variant) IS NOT DISTINCT FROM lower(h.variant))
                                  ORDER BY m.performed_at DESC LIMIT 1)),
         last_session AS (
            SELECT s.exercise_id,
                   json_build_object(
                       'date', min(s.performed_at)::date,
                       'gym', (SELECT name FROM gym WHERE id = min(s.gym_id)),
                       'sets', json_agg(json_build_object('reps', s.reps, 'weight_kg', s.weight_kg, 'duration_s', s.duration_s,
                                                          'rpe', s.rpe, 'rest_s', s.rest_s, 'distance_m', s.distance_m,
                                                          'avg_hr', s.avg_hr, 'max_hr', s.max_hr, 'calories', s.calories,
                                                          'avg_incline_pct', s.avg_incline_pct, 'max_incline_pct', s.max_incline_pct,
                                                          'avg_speed_kmh', s.avg_speed_kmh, 'max_speed_kmh', s.max_speed_kmh,
                                                          'intensity', NULLIF(s.intensity, ''), 'machine', s.machine,
                                                          'variant', s.variant, 'unilateral', s.unilateral, 'load_basis', s.load_basis)
                                                          ORDER BY s.performed_at)) AS last
            FROM picked s
            GROUP BY s.exercise_id)
         SELECT json_build_object(
            'gym', (SELECT json_build_object('id', id, 'name', name, 'equipment', NULLIF(equipment, '')) FROM gym WHERE id = $2),
            'today', current_date,
            'exercises', coalesce((
                SELECT json_agg(json_build_object(
                    'id', e.id, 'name', e.name, 'kind', e.kind, 'muscle_groups', e.muscle_groups,
                    'secondary_muscle_groups', e.secondary_muscle_groups,
                    'equipment', NULLIF(e.equipment, ''), 'last_done', st.last_done, 'days_since', current_date - st.last_done::date,
                    'machine_here', h.machine, 'variant_here', h.variant, 'last_session', ls.last,
                    'variants_used', (SELECT array_agg(DISTINCT variant) FROM mine WHERE exercise_id = e.id AND variant IS NOT NULL),
                    'machines_used', (SELECT array_agg(DISTINCT machine) FROM mine WHERE exercise_id = e.id AND machine IS NOT NULL), 'sets_last_30d', coalesce(st.sets_30d, 0))
                    ORDER BY st.last_done NULLS FIRST, e.name)
                FROM exercise e
                LEFT JOIN stats st ON st.exercise_id = e.id
                LEFT JOIN last_session ls ON ls.exercise_id = e.id
                LEFT JOIN here h ON h.exercise_id = e.id
                WHERE e.owner_sub = $1), '[]'::json),
            'cardio_this_week', (
                SELECT json_build_object('minutes', round(coalesce(sum(s.duration_s), 0) / 60.0),
                                         'distance_km', round((coalesce(sum(s.distance_m), 0) / 1000.0)::numeric, 1))
                FROM mine s JOIN exercise e ON e.id = s.exercise_id
                WHERE e.kind = 'cardio' AND s.performed_at >= date_trunc('week', now())),
            'done_today', coalesce((
                SELECT json_agg(json_build_object('exercise', e.name, 'sets', t.n, 'muscle_groups', e.muscle_groups,
                                         'secondary_muscle_groups', e.secondary_muscle_groups))
                FROM (SELECT exercise_id, count(*) n FROM work WHERE performed_at::date = current_date GROUP BY exercise_id) t
                JOIN exercise e ON e.id = t.exercise_id), '[]'::json))"
    )
    .bind(owner)
    .bind(gym_id)
    .fetch_one(&mut *tx)
    .await?;
    v["muscle_groups"] = muscle_groups_overview(&mut tx, owner).await?;
    let settings = setup::settings_json(&mut tx, owner).await?;
    for k in ["goal", "goals_notes", "priority_muscle_groups", "limitations"] {
        v[k] = settings[k].clone();
    }
    v["body_weight"] = crate::measurement::body_weight_json(&mut tx, owner).await?;
    v["measurements"] = crate::measurement::latest_json(&mut tx, owner).await?;
    v["body_notes"] = crate::body_note::recent_json(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(json!({"overview": v, "closed_stale": closed}))
}

/// The window `[lo, hi)`: either the last `days` days up to now, or `from` (to `to`, default now).
/// Times are parsed in SQL so naive local input uses the session TimeZone.
async fn window(
    tx: &mut Tx,
    days: Option<i64>,
    from: &Option<String>,
    to: &Option<String>,
) -> AppResult<(DateTime<Utc>, DateTime<Utc>)> {
    let (lo, hi): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT coalesce($1::timestamptz, now() - make_interval(days => $3::int)), coalesce($2::timestamptz, now())",
    )
    .bind(from)
    .bind(to)
    .bind(days.map(|d| d as i32))
    .fetch_one(&mut **tx)
    .await?;
    if lo >= hi {
        return Err(user_err("from must be before to"));
    }
    Ok((lo, hi))
}

/// Per muscle group in `[lo, hi)`: weighted sets (primary 1, secondary 0.5), sets per week, local training
/// days and average RPE (primary only), the exercises behind the sets, and all-time days since (primary).
/// Strength exercises only. Shared by training_overview and muscle_group_volume.
async fn muscle_volume(tx: &mut Tx, owner: &str, lo: DateTime<Utc>, hi: DateTime<Utc>) -> AppResult<Value> {
    let groups: Vec<String> = MUSCLE_GROUPS.iter().map(|s| s.to_string()).collect();
    Ok(sqlx::query_scalar(AssertSqlSafe(format!(
        "WITH hits AS (
            SELECT g.muscle_group, g.weight, s.performed_at, s.rpe, e.name AS exercise, {SIDES} AS sides
            FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN exercise e ON e.id = s.exercise_id
            CROSS JOIN LATERAL (SELECT unnest(e.muscle_groups), 1.0
                                UNION ALL SELECT unnest(e.secondary_muscle_groups), 0.5) g(muscle_group, weight)
            WHERE w.owner_sub = $1 AND e.kind = 'strength' AND s.set_type = 'working'),
         win AS (SELECT * FROM hits WHERE performed_at >= $2 AND performed_at < $3),
         span AS (SELECT extract(epoch FROM $3::timestamptz - $2::timestamptz) / 86400 AS days)
         SELECT json_build_object(
            'from', $2::timestamptz, 'to', $3::timestamptz, 'days', round((SELECT days FROM span)::numeric, 1),
            'muscle_groups', json_agg(json_build_object(
                'muscle_group', m.muscle_group, 'sets', x.sets,
                'sets_per_week', round(x.sets / (SELECT days FROM span)::numeric * 7, 1),
                'train_days', (SELECT count(DISTINCT performed_at::date) FROM win
                               WHERE muscle_group = m.muscle_group AND weight = 1),
                'avg_rpe', (SELECT round(avg(rpe)::numeric, 1) FROM win WHERE muscle_group = m.muscle_group AND weight = 1),
                'days_since', (SELECT current_date - max(performed_at)::date FROM hits
                               WHERE muscle_group = m.muscle_group AND weight = 1),
                'from_exercises', coalesce((
                    SELECT json_agg(json_build_object('exercise', exercise, 'sets', sets) ORDER BY sets DESC, exercise)
                    FROM (SELECT exercise, sum(weight * sides) AS sets FROM win WHERE muscle_group = m.muscle_group
                          GROUP BY exercise ORDER BY sets DESC, exercise LIMIT 5) t), '[]'::json))
                ORDER BY m.i))
         FROM unnest($4::text[]) WITH ORDINALITY m(muscle_group, i)
         CROSS JOIN LATERAL (SELECT coalesce(sum(weight * sides), 0) AS sets FROM win WHERE muscle_group = m.muscle_group) x"
    )))
    .bind(owner)
    .bind(lo)
    .bind(hi)
    .bind(&groups)
    .fetch_one(&mut **tx)
    .await?)
}

/// The overview's per-muscle summary: the last 7 and 28 days from `muscle_volume`.
async fn muscle_groups_overview(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    let (lo7, hi) = window(tx, Some(7), &None, &None).await?;
    let lo28 = hi - chrono::Duration::days(28);
    let w7 = muscle_volume(tx, owner, lo7, hi).await?;
    let w28 = muscle_volume(tx, owner, lo28, hi).await?;
    // Both windows list every group in MUSCLE_GROUPS order, so rows pair up by index.
    let rows = (0..MUSCLE_GROUPS.len())
        .map(|i| {
            let (a, b) = (&w7["muscle_groups"][i], &w28["muscle_groups"][i]);
            json!({"muscle_group": b["muscle_group"], "days_since": b["days_since"], "sets_7d": a["sets"],
                   "sets_28d": b["sets"], "train_days_28d": b["train_days"], "avg_rpe_28d": b["avg_rpe"]})
        })
        .collect();
    Ok(Value::Array(rows))
}

// ---------- muscle_group_volume ----------

#[derive(Deserialize, JsonSchema)]
pub struct MuscleVolumeArgs {
    /// The last N days up to now (1-365). Default 7 when from is not given.
    pub days: Option<i64>,
    /// Start of a fixed period, inclusive. RFC 3339 or local ("2026-10-01").
    pub from: Option<String>,
    /// End of a fixed period, exclusive. Default now.
    pub to: Option<String>,
}

pub async fn muscle_group_volume(app: &App, owner: &str, a: MuscleVolumeArgs) -> AppResult<Value> {
    if a.days.is_some() && (a.from.is_some() || a.to.is_some()) {
        return Err(user_err("pass days, or from/to, not both"));
    }
    if a.to.is_some() && a.from.is_none() {
        return Err(user_err("to needs from"));
    }
    if a.days.is_some_and(|d| !(1..=365).contains(&d)) {
        return Err(user_err("days must be between 1 and 365"));
    }
    let days = if a.from.is_none() {
        Some(a.days.unwrap_or(7))
    } else {
        None
    };
    let mut tx = db::begin_read(app, owner).await?;
    let (lo, hi) = window(&mut tx, days, &a.from, &a.to).await?;
    let v = muscle_volume(&mut tx, owner, lo, hi).await?;
    tx.commit().await?;
    Ok(v)
}

// ---------- search ----------

#[derive(Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Free text: names (typos ok) and words in notes/descriptions.
    pub query: String,
    pub limit: Option<i64>,
}

pub async fn search(app: &App, owner: &str, a: SearchArgs) -> AppResult<Value> {
    let lim = limit(a.limit.or(Some(20)));
    let mut tx = db::begin_read(app, owner).await?;
    let mut names = Vec::new();
    for kind in [Kind::Gym, Kind::Exercise] {
        let cands = resolve::candidates(&mut tx, owner, kind).await?;
        for (score, id, name) in resolve::rank(&a.query, &cands).into_iter().filter(|r| r.0 >= 0.5) {
            names.push(json!({"type": kind.table(), "id": id, "name": name, "score": (score.min(1.0) * 100.0).round() / 100.0}));
        }
    }
    names.sort_by(|x, y| {
        y["score"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&x["score"].as_f64().unwrap_or(0.0))
    });
    names.truncate(lim as usize);
    let notes: Value = sqlx::query_scalar(
        "WITH q AS (SELECT websearch_to_tsquery('simple', $2) AS q)
         SELECT coalesce(json_agg(h ORDER BY h->>'rank' DESC), '[]'::json) FROM (
            SELECT json_build_object('type', 'gym', 'id', g.id, 'name', g.name, 'text', g.notes, 'rank', ts_rank(g.notes_tsv, q.q)) h
            FROM gym g, q WHERE g.owner_sub = $1 AND g.notes_tsv @@ q.q
            UNION ALL
            SELECT json_build_object('type', 'exercise', 'id', e.id, 'name', e.name, 'text', e.description, 'rank', ts_rank(e.description_tsv, q.q))
            FROM exercise e, q WHERE e.owner_sub = $1 AND e.description_tsv @@ q.q
            UNION ALL
            SELECT json_build_object('type', 'gym_equipment', 'id', g.id, 'name', g.name, 'text', g.equipment,
                                     'rank', ts_rank(g.equipment_tsv, q.q))
            FROM gym g, q WHERE g.owner_sub = $1 AND g.equipment_tsv @@ q.q
            UNION ALL
            SELECT json_build_object('type', 'exercise_equipment', 'id', e.id, 'name', e.name, 'text', e.equipment,
                                     'rank', ts_rank(e.equipment_tsv, q.q))
            FROM exercise e, q WHERE e.owner_sub = $1 AND e.equipment_tsv @@ q.q
            UNION ALL
            SELECT json_build_object('type', 'workout', 'id', w.id, 'started_at', w.started_at, 'text', w.notes, 'rank', ts_rank(w.notes_tsv, q.q))
            FROM workout w, q WHERE w.owner_sub = $1 AND w.notes_tsv @@ q.q
            UNION ALL
            SELECT json_build_object('type', 'set', 'id', s.id, 'workout_id', s.workout_id, 'exercise', e.name,
                                     'performed_at', s.performed_at, 'reps', s.reps, 'weight_kg', s.weight_kg,
                                     'text', s.notes, 'rank', ts_rank(s.notes_tsv, q.q))
            FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN exercise e ON e.id = s.exercise_id, q
            WHERE w.owner_sub = $1 AND s.notes_tsv @@ q.q
            LIMIT $3) x",
    )
    .bind(owner)
    .bind(&a.query)
    .bind(lim)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"names": names, "notes": notes}))
}

// ---------- set_search ----------

#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum SetOrder {
    #[default]
    PerformedAt,
    Weight,
    Reps,
    Volume,
    E1rm,
    /// Cardio: longest first.
    Distance,
    /// Cardio: seconds per km; pass desc false for fastest first.
    Pace,
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SetSearchArgs {
    pub exercise: Option<String>,
    /// Matches primary muscles. Coarse names work too: back, shoulders, legs, arms.
    pub muscle_group: Option<String>,
    /// Only exercises whose equipment text contains this, e.g. "dumbbell".
    pub equipment: Option<String>,
    /// Only sets on a machine whose name contains this, e.g. "technogym".
    pub machine: Option<String>,
    /// Only sets whose grip or variant contains this, e.g. "neutral".
    pub variant: Option<String>,
    pub gym: Option<String>,
    /// Inclusive. RFC 3339 or a local date/time ("2026-10-01").
    pub from: Option<String>,
    /// Exclusive.
    pub to: Option<String>,
    pub min_weight_kg: Option<f64>,
    pub max_weight_kg: Option<f64>,
    pub min_reps: Option<i32>,
    pub max_reps: Option<i32>,
    /// Default: all types.
    pub set_type: Option<workout::SetType>,
    #[serde(default)]
    pub order_by: SetOrder,
    /// Default true (newest / heaviest first).
    pub desc: Option<bool>,
    pub limit: Option<i64>,
}

pub async fn set_search(app: &App, owner: &str, a: SetSearchArgs) -> AppResult<Value> {
    let groups = expand_opt(&a.muscle_group)?;
    let mut tx = db::begin_read(app, owner).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let gym = opt_id(&mut tx, owner, Kind::Gym, &a.gym).await?;

    let mut q: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT json_build_object('set_id', s.id, 'workout_id', w.id, 'gym', g.name, 'exercise', e.name,
                'performed_at', s.performed_at, 'reps', s.reps, 'weight_kg', s.weight_kg, 'duration_s', s.duration_s,
                'rpe', s.rpe, 'rest_s', s.rest_s, 'distance_m', s.distance_m, 'intensity', NULLIF(s.intensity, ''),
                'avg_hr', s.avg_hr, 'max_hr', s.max_hr, 'calories', s.calories,
                'avg_incline_pct', s.avg_incline_pct, 'max_incline_pct', s.max_incline_pct,
                'avg_speed_kmh', s.avg_speed_kmh, 'max_speed_kmh', s.max_speed_kmh,
                'set_type', s.set_type, 'unilateral', s.unilateral, 'load_basis', s.load_basis,
                'machine', s.machine, 'variant', s.variant, 'time_estimated', s.time_estimated, 'pace_s_per_km', round({PACE}::numeric), 'volume_kg', {VOLUME}, 'e1rm_kg', round({E1RM}::numeric, 1), 'notes', NULLIF(s.notes, ''))
         FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN gym g ON g.id = w.gym_id
         JOIN exercise e ON e.id = s.exercise_id
         WHERE w.owner_sub = "
    ));
    q.push_bind(owner);
    if let Some(id) = exercise {
        q.push(" AND s.exercise_id = ").push_bind(id);
    }
    if let Some(g) = groups {
        q.push(" AND e.muscle_groups && ").push_bind(g);
    }
    if let Some(eq) = &a.equipment {
        q.push(" AND e.equipment ILIKE '%' || ")
            .push_bind(eq.clone())
            .push(" || '%'");
    }
    if let Some(m) = &a.machine {
        q.push(" AND s.machine ILIKE '%' || ")
            .push_bind(m.clone())
            .push(" || '%'");
    }
    if let Some(v) = &a.variant {
        q.push(" AND s.variant ILIKE '%' || ")
            .push_bind(v.clone())
            .push(" || '%'");
    }
    if let Some(id) = gym {
        q.push(" AND w.gym_id = ").push_bind(id);
    }
    if let Some(f) = &a.from {
        q.push(" AND s.performed_at >= ")
            .push_bind(f.clone())
            .push("::timestamptz");
    }
    if let Some(t) = &a.to {
        q.push(" AND s.performed_at < ")
            .push_bind(t.clone())
            .push("::timestamptz");
    }
    if let Some(v) = a.min_weight_kg {
        q.push(" AND s.weight_kg >= ").push_bind(v);
    }
    if let Some(v) = a.max_weight_kg {
        q.push(" AND s.weight_kg <= ").push_bind(v);
    }
    if let Some(v) = a.min_reps {
        q.push(" AND s.reps >= ").push_bind(v);
    }
    if let Some(v) = a.max_reps {
        q.push(" AND s.reps <= ").push_bind(v);
    }
    if let Some(t) = a.set_type {
        q.push(" AND s.set_type = ").push_bind(t.as_str());
    }
    let key = match a.order_by {
        SetOrder::PerformedAt => "s.performed_at".to_owned(),
        SetOrder::Weight => "s.weight_kg".to_owned(),
        SetOrder::Reps => "s.reps".to_owned(),
        SetOrder::Volume => VOLUME.to_owned(),
        SetOrder::E1rm => E1RM.to_owned(),
        SetOrder::Distance => "s.distance_m".to_owned(),
        SetOrder::Pace => PACE.to_owned(),
    };
    let dir = if a.desc.unwrap_or(true) {
        "DESC NULLS LAST"
    } else {
        "ASC NULLS LAST"
    };
    q.push(format!(" ORDER BY {key} {dir}, s.performed_at DESC LIMIT "))
        .push_bind(limit(a.limit));

    let rows: Vec<Value> = q.build_query_scalar().fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Value::Array(rows))
}

// ---------- progress ----------

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    TopWeight,
    E1rm,
    Volume,
    Sets,
    Reps,
    /// Cardio: total metres.
    Distance,
    /// Cardio: total seconds.
    Duration,
    /// Cardio: best (lowest) seconds per km.
    Pace,
    /// Cardio: average heart rate over time (sets weighted by duration_s), bpm.
    /// Sets without duration_s are left out: their share of the time is unknown.
    AvgHr,
    /// Cardio: average speed over time (sets weighted by duration_s), km/h. Sets without duration_s are left out.
    AvgSpeed,
    /// Cardio: total kcal.
    Calories,
}

#[derive(Deserialize, JsonSchema, Clone, Copy, Default)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Day,
    #[default]
    Week,
    Month,
}

#[derive(Deserialize, JsonSchema)]
pub struct ProgressArgs {
    /// Give exactly one of exercise or muscle_group.
    pub exercise: Option<String>,
    /// Matches primary muscles. Coarse names work too: back, shoulders, legs, arms.
    pub muscle_group: Option<String>,
    pub metric: Metric,
    /// Only sets on a machine whose name contains this. With an exercise, points are already split per machine and variant.
    pub machine: Option<String>,
    /// Only sets whose grip or variant contains this.
    pub variant: Option<String>,
    /// Default week (ISO, Monday start, local time).
    #[serde(default)]
    pub bucket: Bucket,
    pub from: Option<String>,
    pub to: Option<String>,
}

pub async fn progress(app: &App, owner: &str, a: ProgressArgs) -> AppResult<Value> {
    if a.exercise.is_some() == a.muscle_group.is_some() {
        return Err(user_err("give exactly one of exercise or muscle_group"));
    }
    let groups = expand_opt(&a.muscle_group)?;
    let mut tx = db::begin_read(app, owner).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let metric = match a.metric {
        Metric::TopWeight => format!("max({LOAD})"),
        Metric::E1rm => format!("round(max({E1RM})::numeric, 1)"),
        Metric::Volume => format!("sum({VOLUME})"),
        Metric::Sets => format!("sum({SIDES})"),
        Metric::Reps => format!("sum(s.reps * {SIDES})"),
        Metric::Distance => "sum(s.distance_m)".to_owned(),
        Metric::Duration => "sum(s.duration_s)".to_owned(),
        Metric::Pace => format!("round(min({PACE})::numeric)"),
        Metric::AvgHr => "round(sum(s.avg_hr * s.duration_s::numeric) / NULLIF(sum(s.duration_s) FILTER (WHERE s.avg_hr IS NOT NULL), 0))".to_owned(),
        Metric::AvgSpeed => "round((sum(s.avg_speed_kmh * s.duration_s) / NULLIF(sum(s.duration_s) FILTER (WHERE s.avg_speed_kmh IS NOT NULL), 0))::numeric, 1)".to_owned(),
        Metric::Calories => "sum(s.calories)".to_owned(),
    };
    let bucket = match a.bucket {
        Bucket::Day => "day",
        Bucket::Week => "week",
        Bucket::Month => "month",
    };
    // One exercise: a point per machine and variant, since machine weights don't compare across brands
    // and a different grip is a different lift.
    let (machine, variant, machine_key) = if exercise.is_some() {
        ("min(s.machine)", "min(s.variant)", SETUP_KEY)
    } else {
        ("NULL::text", "NULL::text", "NULL::text")
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg(json_build_object('period_start', b, 'machine', machine, 'variant', variant, 'value', v)
                                  ORDER BY machine_key NULLS FIRST, b), '[]'::json) FROM (
            SELECT date_trunc($2, s.performed_at)::date AS b, {machine_key} AS machine_key, {machine} AS machine, {variant} AS variant, {metric} AS v
            FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN exercise e ON e.id = s.exercise_id
            WHERE w.owner_sub = $1 AND s.set_type = 'working'
              AND ($3::bigint IS NULL OR s.exercise_id = $3)
              AND ($4::text[] IS NULL OR e.muscle_groups && $4)
              AND ($5::timestamptz IS NULL OR s.performed_at >= $5::timestamptz)
              AND ($6::timestamptz IS NULL OR s.performed_at < $6::timestamptz)
              AND ($7::text IS NULL OR s.machine ILIKE '%' || $7 || '%')
              AND ($8::text IS NULL OR s.variant ILIKE '%' || $8 || '%')
            GROUP BY 1, 2) x"
    )))
    .bind(owner)
    .bind(bucket)
    .bind(exercise)
    .bind(&groups)
    .bind(&a.from)
    .bind(&a.to)
    .bind(&a.machine)
    .bind(&a.variant)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"bucket": bucket, "series": v}))
}

// ---------- personal_records ----------

#[derive(Deserialize, JsonSchema)]
pub struct RecordsArgs {
    /// Omit for every exercise.
    pub exercise: Option<String>,
}

pub async fn personal_records(app: &App, owner: &str, a: RecordsArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "WITH mine AS (
            -- records are per machine (stack weights don't compare across brands) and per variant
            SELECT s.*, {SETUP_KEY} AS machine_key FROM workout_set s JOIN workout w ON w.id = s.workout_id
            WHERE w.owner_sub = $1 AND s.set_type = 'working' AND ($2::bigint IS NULL OR s.exercise_id = $2)),
         heaviest AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('weight_kg', {LOAD}, 'reps', reps, 'performed_at', performed_at, 'set_id', id) AS r
            FROM mine s WHERE weight_kg IS NOT NULL ORDER BY exercise_id, machine_key, {LOAD} DESC, reps DESC NULLS LAST, performed_at),
         best_e1rm AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('e1rm_kg', round({E1RM}::numeric, 1), 'weight_kg', {LOAD}, 'reps', reps,
                                     'performed_at', performed_at, 'set_id', id) AS r
            FROM mine s WHERE weight_kg IS NOT NULL AND reps > 0 ORDER BY exercise_id, machine_key, {E1RM} DESC, performed_at),
         reps_at AS (
            SELECT exercise_id, machine_key, json_agg(json_build_object('weight_kg', weight_kg, 'reps', best) ORDER BY weight_kg) AS r
            FROM (SELECT exercise_id, machine_key, {LOAD} AS weight_kg, max(reps) AS best FROM mine s
                  WHERE weight_kg IS NOT NULL AND reps IS NOT NULL GROUP BY 1, 2, 3) t GROUP BY exercise_id, machine_key),
         vol AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('volume_kg', v, 'workout_id', workout_id, 'date', d) AS r
            FROM (SELECT exercise_id, machine_key, workout_id, sum({VOLUME}) AS v, min(performed_at)::date AS d FROM mine s
                  GROUP BY exercise_id, machine_key, workout_id) t
            WHERE v IS NOT NULL ORDER BY exercise_id, machine_key, v DESC),
         longest_distance AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('distance_m', distance_m, 'duration_s', duration_s, 'performed_at', performed_at, 'set_id', id) AS r
            FROM mine WHERE distance_m > 0 ORDER BY exercise_id, machine_key, distance_m DESC, performed_at),
         best_pace AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('pace_s_per_km', round({PACE}::numeric), 'distance_m', distance_m, 'duration_s', duration_s,
                                     'performed_at', performed_at, 'set_id', id) AS r
            FROM mine s WHERE distance_m > 0 AND duration_s > 0 ORDER BY exercise_id, machine_key, {PACE}, performed_at),
         longest_duration AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('duration_s', duration_s, 'performed_at', performed_at, 'set_id', id) AS r
            FROM mine WHERE duration_s > 0 ORDER BY exercise_id, machine_key, duration_s DESC, performed_at)
         SELECT coalesce(json_agg(json_build_object(
                    'exercise', e.name, 'machine', m.machine, 'variant', m.variant, 'heaviest_set', h.r, 'best_e1rm', b.r,
                    'max_reps_by_weight_kg', ra.r, 'best_volume_workout', vo.r,
                    'longest_distance', ld.r, 'best_pace', bp.r, 'longest_duration', lu.r) ORDER BY e.name, m.machine_key), '[]'::json)
         FROM exercise e
         JOIN (SELECT exercise_id, machine_key, min(machine) AS machine, min(variant) AS variant FROM mine
               GROUP BY exercise_id, machine_key) m ON m.exercise_id = e.id
         LEFT JOIN heaviest h ON h.exercise_id = e.id AND h.machine_key = m.machine_key
         LEFT JOIN best_e1rm b ON b.exercise_id = e.id AND b.machine_key = m.machine_key
         LEFT JOIN reps_at ra ON ra.exercise_id = e.id AND ra.machine_key = m.machine_key
         LEFT JOIN vol vo ON vo.exercise_id = e.id AND vo.machine_key = m.machine_key
         LEFT JOIN longest_distance ld ON ld.exercise_id = e.id AND ld.machine_key = m.machine_key
         LEFT JOIN best_pace bp ON bp.exercise_id = e.id AND bp.machine_key = m.machine_key
         LEFT JOIN longest_duration lu ON lu.exercise_id = e.id AND lu.machine_key = m.machine_key"
    )))
    .bind(owner)
    .bind(exercise)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

// ---------- exercise_alternatives ----------

#[derive(Deserialize, JsonSchema)]
pub struct AlternativesArgs {
    pub exercise: String,
    /// Default: the gym of the open workout. Its equipment text is returned so you can judge what's possible there.
    pub gym: Option<String>,
}

pub async fn exercise_alternatives(app: &App, owner: &str, a: AlternativesArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let exercise = resolve::id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let gym = gym_or_current(&mut tx, owner, &a.gym).await?;
    let v: Value = sqlx::query_scalar(
        "WITH base AS (SELECT muscle_groups, secondary_muscle_groups AS secondary FROM exercise WHERE id = $3)
         SELECT json_build_object(
            'gym', (SELECT json_build_object('id', id, 'name', name, 'equipment', NULLIF(equipment, ''))
                    FROM gym WHERE id = $2 AND owner_sub = $1),
            'alternatives', coalesce((
                SELECT json_agg(json_build_object(
                    'id', x.id, 'name', x.name, 'muscle_groups', x.muscle_groups, 'shared_muscle_groups', x.shared,
                    'shared_secondary', x.shared_secondary, 'equipment', NULLIF(x.equipment, ''))
                    ORDER BY cardinality(x.shared) DESC, cardinality(x.shared_secondary) DESC, x.name)
                FROM (
                    SELECT e.id, e.name, e.muscle_groups, e.equipment,
                           ARRAY(SELECT unnest(e.muscle_groups) INTERSECT SELECT unnest(base.muscle_groups)) AS shared,
                           -- any other overlap, primary or secondary on either side
                           ARRAY(SELECT unnest(e.muscle_groups || e.secondary_muscle_groups)
                                 INTERSECT SELECT unnest(base.muscle_groups || base.secondary)
                                 EXCEPT SELECT unnest(e.muscle_groups) INTERSECT SELECT unnest(base.muscle_groups))
                               AS shared_secondary
                    FROM exercise e CROSS JOIN base
                    WHERE e.owner_sub = $1 AND e.id <> $3 AND e.muscle_groups && base.muscle_groups) x), '[]'::json))",
    )
    .bind(owner)
    .bind(gym)
    .bind(exercise)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

// ---------- workout_list ----------

#[derive(Deserialize, JsonSchema)]
pub struct WorkoutListArgs {
    pub gym: Option<String>,
    /// Only workouts that include this exercise.
    pub exercise: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Default 10, max 50.
    pub limit: Option<i64>,
}

pub async fn workout_list(app: &App, owner: &str, a: WorkoutListArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let gym = opt_id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT w.id FROM workout w
         WHERE w.owner_sub = $1
           AND ($2::bigint IS NULL OR w.gym_id = $2)
           AND ($3::bigint IS NULL OR EXISTS (SELECT 1 FROM workout_set s WHERE s.workout_id = w.id AND s.exercise_id = $3))
           AND ($4::timestamptz IS NULL OR w.started_at >= $4::timestamptz)
           AND ($5::timestamptz IS NULL OR w.started_at < $5::timestamptz)
         ORDER BY w.started_at DESC LIMIT $6",
    )
    .bind(owner)
    .bind(gym)
    .bind(exercise)
    .bind(&a.from)
    .bind(&a.to)
    .bind(a.limit.unwrap_or(10).clamp(1, 50))
    .fetch_all(&mut *tx)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        out.push(workout::workout_json(&mut tx, owner, id).await?);
    }
    tx.commit().await?;
    Ok(Value::Array(out))
}
