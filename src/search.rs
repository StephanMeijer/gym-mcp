//! Read-only search and analysis. All dates are by performed_at, in the user's timezone
//! (the transaction's TimeZone is set by `db::begin`).

use schemars::JsonSchema;
use serde::Deserialize;
// Dynamic SQL only interpolates compile-time constants and enum-mapped literals; user input is always bound.
use serde_json::{Value, json};
use sqlx::AssertSqlSafe;
use sqlx::{Postgres, QueryBuilder};

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::resolve::{self, Kind};
use crate::setup::{MUSCLE_GROUPS, expand_opt};
use crate::workout;

const E1RM: &str = "(s.weight_kg * (1 + s.reps / 30.0))"; // Epley
const VOLUME: &str = "(s.weight_kg * s.reps)";
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
         stats AS (SELECT exercise_id, max(performed_at) AS last_done,
                          count(*) FILTER (WHERE performed_at >= now() - interval '30 days') AS sets_30d
                   FROM mine GROUP BY exercise_id),
         -- the machine last used for each exercise at this gym
         here AS (SELECT DISTINCT ON (exercise_id) exercise_id, machine FROM mine
                  WHERE gym_id = $2 AND machine IS NOT NULL ORDER BY exercise_id, performed_at DESC),
         -- the last session on that machine at any gym (same brand compares across gyms), else the last session
         picked AS (
            SELECT s.* FROM mine s LEFT JOIN here h ON h.exercise_id = s.exercise_id
            WHERE (h.machine IS NULL OR lower(s.machine) = lower(h.machine))
              AND s.workout_id = (SELECT m.workout_id FROM mine m
                                  WHERE m.exercise_id = s.exercise_id AND (h.machine IS NULL OR lower(m.machine) = lower(h.machine))
                                  ORDER BY m.performed_at DESC LIMIT 1)),
         last_session AS (
            SELECT s.exercise_id,
                   json_build_object(
                       'date', min(s.performed_at)::date,
                       'gym', (SELECT name FROM gym WHERE id = min(s.gym_id)),
                       'sets', json_agg(json_build_object('reps', s.reps, 'weight_kg', s.weight_kg, 'duration_s', s.duration_s,
                                                          'rpe', s.rpe, 'rest_s', s.rest_s, 'distance_m', s.distance_m,
                                                          'intensity', NULLIF(s.intensity, ''), 'machine', s.machine)
                                                          ORDER BY s.performed_at)) AS last
            FROM picked s
            GROUP BY s.exercise_id)
         SELECT json_build_object(
            'gym', (SELECT json_build_object('id', id, 'name', name, 'equipment', NULLIF(equipment, '')) FROM gym WHERE id = $2),
            'today', current_date,
            'goal', (SELECT goal FROM user_settings WHERE owner_sub = $1),
            'exercises', coalesce((
                SELECT json_agg(json_build_object(
                    'id', e.id, 'name', e.name, 'kind', e.kind, 'muscle_groups', e.muscle_groups,
                    'secondary_muscle_groups', e.secondary_muscle_groups,
                    'equipment', NULLIF(e.equipment, ''), 'last_done', st.last_done, 'days_since', current_date - st.last_done::date,
                    'machine_here', h.machine, 'last_session', ls.last,
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
                FROM (SELECT exercise_id, count(*) n FROM mine WHERE performed_at::date = current_date GROUP BY exercise_id) t
                JOIN exercise e ON e.id = t.exercise_id), '[]'::json))"
    )
    .bind(owner)
    .bind(gym_id)
    .fetch_one(&mut *tx)
    .await?;
    v["muscle_groups"] = muscle_groups_json(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(json!({"overview": v, "closed_stale": closed}))
}

/// Per muscle group: recency, weighted sets (primary 1, secondary 0.5) over rolling 7 and 28 days,
/// training days and average RPE. Strength exercises only; local days via the session TimeZone.
async fn muscle_groups_json(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    let groups: Vec<String> = MUSCLE_GROUPS.iter().map(|s| s.to_string()).collect();
    Ok(sqlx::query_scalar(
        "WITH hits AS (
            SELECT g.muscle_group, g.weight, s.performed_at, s.rpe
            FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN exercise e ON e.id = s.exercise_id
            CROSS JOIN LATERAL (SELECT unnest(e.muscle_groups), 1.0
                                UNION ALL SELECT unnest(e.secondary_muscle_groups), 0.5) g(muscle_group, weight)
            WHERE w.owner_sub = $1 AND e.kind = 'strength')
         SELECT json_object_agg(muscle_group, stats ORDER BY i) FROM (
            SELECT m.muscle_group, m.i, json_build_object(
                'days_since', current_date - (max(h.performed_at) FILTER (WHERE h.weight = 1))::date,
                'sets_7d', coalesce(sum(h.weight) FILTER (WHERE h.performed_at >= now() - interval '7 days'), 0),
                'sets_28d', coalesce(sum(h.weight) FILTER (WHERE h.performed_at >= now() - interval '28 days'), 0),
                'train_days_28d', count(DISTINCT h.performed_at::date)
                                      FILTER (WHERE h.weight = 1 AND h.performed_at >= now() - interval '28 days'),
                'avg_rpe_28d', round((avg(h.rpe)
                                      FILTER (WHERE h.weight = 1 AND h.performed_at >= now() - interval '28 days'))::numeric, 1)
            ) AS stats
            FROM unnest($2::text[]) WITH ORDINALITY m(muscle_group, i)
            LEFT JOIN hits h ON h.muscle_group = m.muscle_group
            GROUP BY m.muscle_group, m.i) x",
    )
    .bind(owner)
    .bind(&groups)
    .fetch_one(&mut **tx)
    .await?)
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
    pub gym: Option<String>,
    /// Inclusive. RFC 3339 or a local date/time ("2026-10-01").
    pub from: Option<String>,
    /// Exclusive.
    pub to: Option<String>,
    pub min_weight_kg: Option<f64>,
    pub max_weight_kg: Option<f64>,
    pub min_reps: Option<i32>,
    pub max_reps: Option<i32>,
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
                'machine', s.machine, 'pace_s_per_km', round({PACE}::numeric), 'volume_kg', {VOLUME}, 'e1rm_kg', round({E1RM}::numeric, 1), 'notes', NULLIF(s.notes, ''))
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
    /// Only sets on a machine whose name contains this. With an exercise, points are already split per machine.
    pub machine: Option<String>,
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
        Metric::TopWeight => "max(s.weight_kg)".to_owned(),
        Metric::E1rm => format!("round(max({E1RM})::numeric, 1)"),
        Metric::Volume => format!("sum({VOLUME})"),
        Metric::Sets => "count(*)".to_owned(),
        Metric::Reps => "sum(s.reps)".to_owned(),
        Metric::Distance => "sum(s.distance_m)".to_owned(),
        Metric::Duration => "sum(s.duration_s)".to_owned(),
        Metric::Pace => format!("round(min({PACE})::numeric)"),
    };
    let bucket = match a.bucket {
        Bucket::Day => "day",
        Bucket::Week => "week",
        Bucket::Month => "month",
    };
    // One exercise: a point per machine, since machine weights don't compare across brands.
    let machine = if exercise.is_some() {
        "min(s.machine)"
    } else {
        "NULL::text"
    };
    let machine_key = if exercise.is_some() {
        "lower(s.machine)"
    } else {
        "NULL::text"
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg(json_build_object('period_start', b, 'machine', machine, 'value', v)
                                  ORDER BY machine_key NULLS FIRST, b), '[]'::json) FROM (
            SELECT date_trunc($2, s.performed_at)::date AS b, {machine_key} AS machine_key, {machine} AS machine, {metric} AS v
            FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN exercise e ON e.id = s.exercise_id
            WHERE w.owner_sub = $1
              AND ($3::bigint IS NULL OR s.exercise_id = $3)
              AND ($4::text[] IS NULL OR e.muscle_groups && $4)
              AND ($5::timestamptz IS NULL OR s.performed_at >= $5::timestamptz)
              AND ($6::timestamptz IS NULL OR s.performed_at < $6::timestamptz)
              AND ($7::text IS NULL OR s.machine ILIKE '%' || $7 || '%')
            GROUP BY 1, 2) x"
    )))
    .bind(owner)
    .bind(bucket)
    .bind(exercise)
    .bind(&groups)
    .bind(&a.from)
    .bind(&a.to)
    .bind(&a.machine)
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
            -- records are per machine (stack weights don't compare across brands); '' = unknown machine
            SELECT s.*, coalesce(lower(s.machine), '') AS machine_key FROM workout_set s JOIN workout w ON w.id = s.workout_id
            WHERE w.owner_sub = $1 AND ($2::bigint IS NULL OR s.exercise_id = $2)),
         heaviest AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('weight_kg', weight_kg, 'reps', reps, 'performed_at', performed_at, 'set_id', id) AS r
            FROM mine WHERE weight_kg IS NOT NULL ORDER BY exercise_id, machine_key, weight_kg DESC, reps DESC NULLS LAST, performed_at),
         best_e1rm AS (
            SELECT DISTINCT ON (exercise_id, machine_key) exercise_id, machine_key,
                   json_build_object('e1rm_kg', round({E1RM}::numeric, 1), 'weight_kg', weight_kg, 'reps', reps,
                                     'performed_at', performed_at, 'set_id', id) AS r
            FROM mine s WHERE weight_kg IS NOT NULL AND reps > 0 ORDER BY exercise_id, machine_key, {E1RM} DESC, performed_at),
         reps_at AS (
            SELECT exercise_id, machine_key, json_object_agg(weight_kg, best ORDER BY weight_kg) AS r
            FROM (SELECT exercise_id, machine_key, weight_kg, max(reps) AS best FROM mine WHERE weight_kg IS NOT NULL AND reps IS NOT NULL
                  GROUP BY exercise_id, machine_key, weight_kg) t GROUP BY exercise_id, machine_key),
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
                    'exercise', e.name, 'machine', m.machine, 'heaviest_set', h.r, 'best_e1rm', b.r,
                    'max_reps_by_weight_kg', ra.r, 'best_volume_workout', vo.r,
                    'longest_distance', ld.r, 'best_pace', bp.r, 'longest_duration', lu.r) ORDER BY e.name, m.machine_key), '[]'::json)
         FROM exercise e
         JOIN (SELECT exercise_id, machine_key, min(machine) AS machine FROM mine GROUP BY exercise_id, machine_key) m ON m.exercise_id = e.id
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
