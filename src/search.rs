//! Read-only search and analysis. All dates are by performed_at, in the user's timezone
//! (the transaction's TimeZone is set by `db::begin`).

use schemars::JsonSchema;
use serde::Deserialize;
// Dynamic SQL only interpolates compile-time constants and enum-mapped literals; user input is always bound.
use sqlx::AssertSqlSafe;
use serde_json::{Value, json};
use sqlx::{Postgres, QueryBuilder};

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::resolve::{self, Kind};
use crate::setup::{AVAILABILITY_CTE, MUSCLE_GROUPS};
use crate::workout;

const E1RM: &str = "(s.weight_kg * (1 + s.reps / 30.0))"; // Epley
const VOLUME: &str = "(s.weight_kg * s.reps)";

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
    Ok(sqlx::query_scalar("SELECT gym_id FROM workout WHERE owner_sub = $1 AND finished_at IS NULL")
        .bind(owner)
        .fetch_optional(&mut **tx)
        .await?)
}

// ---------- training_overview ----------

#[derive(Deserialize, JsonSchema)]
pub struct OverviewArgs {
    /// Gym name, alias, or id. Default: the gym of the open workout.
    pub gym: Option<String>,
}

pub async fn training_overview(app: &App, owner: &str, a: OverviewArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let closed = workout::close_stale(app, &mut tx, owner).await?;
    let gym_id = gym_or_current(&mut tx, owner, &a.gym)
        .await?
        .ok_or_else(|| user_err("no open workout; pass a gym"))?;
    let groups: Vec<String> = MUSCLE_GROUPS.iter().map(|s| s.to_string()).collect();
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "WITH {AVAILABILITY_CTE},
         mine AS (SELECT s.* FROM workout_set s JOIN workout w ON w.id = s.workout_id WHERE w.owner_sub = $1),
         stats AS (SELECT exercise_id, max(performed_at) AS last_done,
                          count(*) FILTER (WHERE performed_at >= now() - interval '30 days') AS sets_30d
                   FROM mine GROUP BY exercise_id),
         last_top AS (
            SELECT DISTINCT ON (s.exercise_id) s.exercise_id,
                   json_build_object('weight_kg', s.weight_kg, 'reps', s.reps, 'duration_s', s.duration_s, 'performed_at', s.performed_at) AS top
            FROM mine s
            WHERE s.workout_id = (SELECT m.workout_id FROM mine m WHERE m.exercise_id = s.exercise_id ORDER BY m.performed_at DESC LIMIT 1)
            ORDER BY s.exercise_id, s.weight_kg DESC NULLS LAST, s.reps DESC NULLS LAST)
         SELECT json_build_object(
            'gym', (SELECT json_build_object('id', id, 'name', name, 'inventory_complete', inventory_complete) FROM gym WHERE id = $2),
            'today', current_date,
            'exercises', coalesce((
                SELECT json_agg(json_build_object(
                    'id', e.id, 'name', e.name, 'muscle_groups', e.muscle_groups, 'availability', av.availability,
                    'unknown_equipment', NULLIF(av.unknown_equipment, '{{}}'),
                    'last_done', st.last_done, 'days_since', current_date - st.last_done::date,
                    'last_top_set', lt.top, 'sets_last_30d', coalesce(st.sets_30d, 0))
                    ORDER BY av.availability, st.last_done NULLS FIRST)
                FROM exercise e JOIN avail av ON av.exercise_id = e.id
                LEFT JOIN stats st ON st.exercise_id = e.id
                LEFT JOIN last_top lt ON lt.exercise_id = e.id
                WHERE av.availability <> 'unavailable'), '[]'::json),
            'days_since_muscle_group', (
                SELECT json_object_agg(mg, (SELECT current_date - max(s.performed_at)::date
                                            FROM mine s JOIN exercise e ON e.id = s.exercise_id WHERE mg = ANY(e.muscle_groups)))
                FROM unnest($3::text[]) mg),
            'done_today', coalesce((
                SELECT json_agg(json_build_object('exercise', e.name, 'sets', t.n, 'muscle_groups', e.muscle_groups))
                FROM (SELECT exercise_id, count(*) n FROM mine WHERE performed_at::date = current_date GROUP BY exercise_id) t
                JOIN exercise e ON e.id = t.exercise_id), '[]'::json))"
    )))
    .bind(owner)
    .bind(gym_id)
    .bind(&groups)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"overview": v, "closed_stale": closed}))
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
    let mut tx = db::begin(app, owner).await?;
    let mut names = Vec::new();
    for kind in [Kind::Gym, Kind::Exercise, Kind::Equipment] {
        let cands = resolve::candidates(&mut tx, owner, kind).await?;
        for (score, id, name) in resolve::rank(&a.query, &cands).into_iter().filter(|r| r.0 >= 0.5) {
            names.push(json!({"type": kind.table(), "id": id, "name": name, "score": (score.min(1.0) * 100.0).round() / 100.0}));
        }
    }
    names.sort_by(|x, y| y["score"].as_f64().unwrap_or(0.0).total_cmp(&x["score"].as_f64().unwrap_or(0.0)));
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
}

#[derive(Deserialize, JsonSchema, Default)]
pub struct SetSearchArgs {
    pub exercise: Option<String>,
    pub muscle_group: Option<String>,
    /// Only exercises that use this equipment.
    pub equipment: Option<String>,
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
    let mut tx = db::begin(app, owner).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let equipment = opt_id(&mut tx, owner, Kind::Equipment, &a.equipment).await?;
    let gym = opt_id(&mut tx, owner, Kind::Gym, &a.gym).await?;

    let mut q: QueryBuilder<Postgres> = QueryBuilder::new(format!(
        "SELECT json_build_object('set_id', s.id, 'workout_id', w.id, 'gym', g.name, 'exercise', e.name,
                'performed_at', s.performed_at, 'reps', s.reps, 'weight_kg', s.weight_kg, 'duration_s', s.duration_s,
                'volume_kg', {VOLUME}, 'e1rm_kg', round({E1RM}::numeric, 1), 'notes', NULLIF(s.notes, ''))
         FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN gym g ON g.id = w.gym_id
         JOIN exercise e ON e.id = s.exercise_id
         WHERE w.owner_sub = "
    ));
    q.push_bind(owner);
    if let Some(id) = exercise {
        q.push(" AND s.exercise_id = ").push_bind(id);
    }
    if let Some(mg) = &a.muscle_group {
        q.push(" AND ").push_bind(mg.clone()).push(" = ANY(e.muscle_groups)");
    }
    if let Some(id) = equipment {
        q.push(" AND EXISTS (SELECT 1 FROM exercise_equipment ee WHERE ee.exercise_id = e.id AND ee.equipment_id = ")
            .push_bind(id)
            .push(")");
    }
    if let Some(id) = gym {
        q.push(" AND w.gym_id = ").push_bind(id);
    }
    if let Some(f) = &a.from {
        q.push(" AND s.performed_at >= ").push_bind(f.clone()).push("::timestamptz");
    }
    if let Some(t) = &a.to {
        q.push(" AND s.performed_at < ").push_bind(t.clone()).push("::timestamptz");
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
    };
    let dir = if a.desc.unwrap_or(true) { "DESC NULLS LAST" } else { "ASC NULLS LAST" };
    q.push(format!(" ORDER BY {key} {dir}, s.performed_at DESC LIMIT ")).push_bind(limit(a.limit));

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
    pub muscle_group: Option<String>,
    pub metric: Metric,
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
    let mut tx = db::begin(app, owner).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let metric = match a.metric {
        Metric::TopWeight => "max(s.weight_kg)".to_owned(),
        Metric::E1rm => format!("round(max({E1RM})::numeric, 1)"),
        Metric::Volume => format!("sum({VOLUME})"),
        Metric::Sets => "count(*)".to_owned(),
        Metric::Reps => "sum(s.reps)".to_owned(),
    };
    let bucket = match a.bucket {
        Bucket::Day => "day",
        Bucket::Week => "week",
        Bucket::Month => "month",
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg(json_build_object('period_start', b, 'value', v) ORDER BY b), '[]'::json) FROM (
            SELECT date_trunc($2, s.performed_at)::date AS b, {metric} AS v
            FROM workout_set s JOIN workout w ON w.id = s.workout_id JOIN exercise e ON e.id = s.exercise_id
            WHERE w.owner_sub = $1
              AND ($3::bigint IS NULL OR s.exercise_id = $3)
              AND ($4::text IS NULL OR $4 = ANY(e.muscle_groups))
              AND ($5::timestamptz IS NULL OR s.performed_at >= $5::timestamptz)
              AND ($6::timestamptz IS NULL OR s.performed_at < $6::timestamptz)
            GROUP BY 1) x"
    )))
    .bind(owner)
    .bind(bucket)
    .bind(exercise)
    .bind(&a.muscle_group)
    .bind(&a.from)
    .bind(&a.to)
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
    let mut tx = db::begin(app, owner).await?;
    let exercise = opt_id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "WITH mine AS (
            SELECT s.* FROM workout_set s JOIN workout w ON w.id = s.workout_id
            WHERE w.owner_sub = $1 AND ($2::bigint IS NULL OR s.exercise_id = $2)),
         heaviest AS (
            SELECT DISTINCT ON (exercise_id) exercise_id,
                   json_build_object('weight_kg', weight_kg, 'reps', reps, 'performed_at', performed_at, 'set_id', id) AS r
            FROM mine WHERE weight_kg IS NOT NULL ORDER BY exercise_id, weight_kg DESC, reps DESC NULLS LAST, performed_at),
         best_e1rm AS (
            SELECT DISTINCT ON (exercise_id) exercise_id,
                   json_build_object('e1rm_kg', round({E1RM}::numeric, 1), 'weight_kg', weight_kg, 'reps', reps,
                                     'performed_at', performed_at, 'set_id', id) AS r
            FROM mine s WHERE weight_kg IS NOT NULL AND reps > 0 ORDER BY exercise_id, {E1RM} DESC, performed_at),
         reps_at AS (
            SELECT exercise_id, json_object_agg(weight_kg, best ORDER BY weight_kg) AS r
            FROM (SELECT exercise_id, weight_kg, max(reps) AS best FROM mine WHERE weight_kg IS NOT NULL AND reps IS NOT NULL
                  GROUP BY exercise_id, weight_kg) t GROUP BY exercise_id),
         vol AS (
            SELECT DISTINCT ON (exercise_id) exercise_id,
                   json_build_object('volume_kg', v, 'workout_id', workout_id, 'date', d) AS r
            FROM (SELECT exercise_id, workout_id, sum({VOLUME}) AS v, min(performed_at)::date AS d FROM mine s
                  GROUP BY exercise_id, workout_id) t
            WHERE v IS NOT NULL ORDER BY exercise_id, v DESC)
         SELECT coalesce(json_agg(json_build_object(
                    'exercise', e.name, 'heaviest_set', h.r, 'best_e1rm', b.r,
                    'max_reps_by_weight_kg', ra.r, 'best_volume_workout', vo.r) ORDER BY e.name), '[]'::json)
         FROM exercise e
         JOIN (SELECT DISTINCT exercise_id FROM mine) m ON m.exercise_id = e.id
         LEFT JOIN heaviest h ON h.exercise_id = e.id
         LEFT JOIN best_e1rm b ON b.exercise_id = e.id
         LEFT JOIN reps_at ra ON ra.exercise_id = e.id
         LEFT JOIN vol vo ON vo.exercise_id = e.id"
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
    /// Default: the gym of the open workout. Without either, availability is not checked.
    pub gym: Option<String>,
}

pub async fn exercise_alternatives(app: &App, owner: &str, a: AlternativesArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let exercise = resolve::id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    let gym = gym_or_current(&mut tx, owner, &a.gym).await?.unwrap_or(-1);
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "WITH {AVAILABILITY_CTE},
         base AS (SELECT muscle_groups FROM exercise WHERE id = $3)
         SELECT coalesce(json_agg(json_build_object(
                    'id', x.id, 'name', x.name, 'muscle_groups', x.muscle_groups, 'shared_muscle_groups', x.shared,
                    'availability', x.availability, 'unknown_equipment', NULLIF(x.unknown_equipment, '{{}}'))
                ORDER BY cardinality(x.shared) DESC, x.availability, x.name), '[]'::json)
         FROM (
            SELECT e.id, e.name, e.muscle_groups, av.availability, av.unknown_equipment,
                   ARRAY(SELECT unnest(e.muscle_groups) INTERSECT SELECT unnest(base.muscle_groups)) AS shared
            FROM exercise e CROSS JOIN base
            LEFT JOIN avail av ON av.exercise_id = e.id
            WHERE e.owner_sub = $1 AND e.id <> $3 AND e.muscle_groups && base.muscle_groups
              AND ($2 = -1 OR av.availability <> 'unavailable')) x"
    )))
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
    let mut tx = db::begin(app, owner).await?;
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
