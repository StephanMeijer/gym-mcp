//! Settings, gyms and exercises.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
// Dynamic SQL only interpolates compile-time constants; user input is always bound.
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::resolve::{self, Kind};

pub const MUSCLE_GROUPS: &[&str] = &[
    "chest",
    "lats",
    "upper_back",
    "lower_back",
    "front_delts",
    "side_delts",
    "rear_delts",
    "biceps",
    "triceps",
    "forearms",
    "core",
    "glutes",
    "quads",
    "hamstrings",
    "adductors",
    "abductors",
    "calves",
    "neck",
];

/// Coarse names that work as filters but aren't stored on exercises.
const MUSCLE_GROUP_ALIASES: &[(&str, &[&str])] = &[
    ("back", &["lats", "upper_back", "lower_back"]),
    ("shoulders", &["front_delts", "side_delts", "rear_delts"]),
    (
        "legs",
        &["quads", "hamstrings", "glutes", "adductors", "abductors", "calves"],
    ),
    ("arms", &["biceps", "triceps", "forearms"]),
];

fn unknown_group(g: &str) -> crate::db::AppError {
    let aliases: Vec<&str> = MUSCLE_GROUP_ALIASES.iter().map(|(a, _)| *a).collect();
    user_err(format!(
        "unknown muscle group '{g}'; use one of {} (filters also accept {})",
        MUSCLE_GROUPS.join(", "),
        aliases.join(", ")
    ))
}

/// A filter value: one muscle group, or a coarse alias expanded to its groups.
pub fn expand_muscle_group(g: &str) -> AppResult<Vec<String>> {
    if MUSCLE_GROUPS.contains(&g) {
        return Ok(vec![g.to_owned()]);
    }
    match MUSCLE_GROUP_ALIASES.iter().find(|(a, _)| *a == g) {
        Some((_, groups)) => Ok(groups.iter().map(|s| s.to_string()).collect()),
        None => Err(unknown_group(g)),
    }
}

pub fn expand_opt(g: &Option<String>) -> AppResult<Option<Vec<String>>> {
    g.as_deref().map(expand_muscle_group).transpose()
}

fn check_muscle_groups(primary: &[String], secondary: &[String]) -> AppResult<()> {
    if let Some(bad) = primary
        .iter()
        .chain(secondary)
        .find(|g| !MUSCLE_GROUPS.contains(&g.as_str()))
    {
        return Err(match MUSCLE_GROUP_ALIASES.iter().find(|(a, _)| *a == bad) {
            Some((a, groups)) => user_err(format!("'{a}' is too coarse to store; pick from {}", groups.join(", "))),
            None => unknown_group(bad),
        });
    }
    if let Some(both) = primary.iter().find(|g| secondary.contains(g)) {
        return Err(user_err(format!("'{both}' can't be both primary and secondary")));
    }
    Ok(())
}

// ---------- settings ----------

/// All settings, plus now/today in the user's timezone. Also merged into training_overview.
pub async fn settings_json(tx: &mut Tx, owner: &str) -> AppResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT json_build_object('timezone', current_setting('TimeZone'), 'now', now(), 'today', current_date,
                                  'goal', us.goal, 'goals_notes', us.goals_notes,
                                  'priority_muscle_groups', us.priority_muscle_groups, 'limitations', us.limitations)
         FROM (SELECT 1) one LEFT JOIN user_settings us ON us.owner_sub = $1",
    )
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?)
}

pub async fn settings_get(app: &App, owner: &str) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let v = settings_json(&mut tx, owner).await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum Goal {
    Strength,
    Hypertrophy,
    Endurance,
    General,
}

impl Goal {
    fn as_str(self) -> &'static str {
        match self {
            Goal::Strength => "strength",
            Goal::Hypertrophy => "hypertrophy",
            Goal::Endurance => "endurance",
            Goal::General => "general",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct SettingsArgs {
    /// IANA timezone, e.g. "Europe/Amsterdam". Used for "today", weeks (ISO, Monday) and months, and for reading local times.
    pub timezone: Option<String>,
    /// What training is for; recommendations use it for rep ranges and progression.
    pub goal: Option<Goal>,
    /// Free-text goals and plans, including events and deadlines ("Hyrox on 14 March, bench 100 kg by summer").
    /// "" clears it.
    pub goals_notes: Option<String>,
    /// Muscle groups to prioritise; coarse names (back, shoulders, legs, arms) expand. [] clears it.
    pub priority_muscle_groups: Option<Vec<String>>,
    /// Injuries or limitations to work around ("left shoulder: no overhead pressing"). "" clears it.
    pub limitations: Option<String>,
}

pub async fn settings_update(app: &App, owner: &str, a: SettingsArgs) -> AppResult<Value> {
    if a.timezone.is_none()
        && a.goal.is_none()
        && a.goals_notes.is_none()
        && a.priority_muscle_groups.is_none()
        && a.limitations.is_none()
    {
        return Err(user_err("pass at least one setting"));
    }
    // Expand coarse names and drop duplicates, keeping the order given.
    let priority = match &a.priority_muscle_groups {
        Some(groups) => {
            let mut out: Vec<String> = Vec::new();
            for g in groups {
                for e in expand_muscle_group(g)? {
                    if !out.contains(&e) {
                        out.push(e);
                    }
                }
            }
            Some(out)
        }
        None => None,
    };
    let mut tx = db::begin(app, owner).await?;
    if let Some(tz) = &a.timezone {
        // Postgres validates the name against its own tz database (22023 → user error).
        sqlx::query("SELECT set_config('TimeZone', $1, true)")
            .bind(tz)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE user_settings SET timezone = coalesce($2, timezone), goal = coalesce($3, goal),
            goals_notes = CASE WHEN $4::text IS NULL THEN goals_notes ELSE NULLIF(btrim($4), '') END,
            priority_muscle_groups = CASE WHEN $5::text[] IS NULL THEN priority_muscle_groups ELSE NULLIF($5, '{}') END,
            limitations = CASE WHEN $6::text IS NULL THEN limitations ELSE NULLIF(btrim($6), '') END
         WHERE owner_sub = $1",
    )
    .bind(owner)
    .bind(&a.timezone)
    .bind(a.goal.map(Goal::as_str))
    .bind(&a.goals_notes)
    .bind(&priority)
    .bind(&a.limitations)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    settings_get(app, owner).await
}

// ---------- gyms ----------

#[derive(Deserialize, JsonSchema)]
pub struct GymCreateArgs {
    pub name: String,
    /// Other names you use for it ("ndsm", "trainmore noord").
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Free text: what the gym has, and what it lacks if that's known, e.g.
    /// "2 leg presses (max 300 kg), dumbbells 2-40 kg in 2 kg steps, cable towers, mats. No sled."
    pub equipment: Option<String>,
    pub notes: Option<String>,
}

pub async fn gym_create(app: &App, owner: &str, a: GymCreateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO gym (owner_sub, name, aliases, equipment, notes)
         VALUES ($1, btrim($2), $3, coalesce($4, ''), coalesce($5, '')) RETURNING id",
    )
    .bind(owner)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.equipment)
    .bind(&a.notes)
    .fetch_one(&mut *tx)
    .await?;
    let v = gym_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct GymUpdateArgs {
    /// Gym name, alias, or id.
    pub gym: String,
    pub name: Option<String>,
    /// Replaces the alias list.
    pub aliases: Option<Vec<String>>,
    /// Replaces the whole equipment text. To add something, use equipment_append.
    pub equipment: Option<String>,
    /// Adds this as a new line under the existing equipment text.
    pub equipment_append: Option<String>,
    /// Replaces the whole notes text. To add something, use notes_append.
    pub notes: Option<String>,
    /// Adds this as a new line under the existing notes.
    pub notes_append: Option<String>,
}

pub async fn gym_update(app: &App, owner: &str, a: GymUpdateArgs) -> AppResult<Value> {
    if a.equipment.is_some() && a.equipment_append.is_some() {
        return Err(user_err("pass equipment or equipment_append, not both"));
    }
    if a.notes.is_some() && a.notes_append.is_some() {
        return Err(user_err("pass notes or notes_append, not both"));
    }
    let mut tx = db::begin(app, owner).await?;
    let id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    sqlx::query(
        "UPDATE gym SET name = coalesce(btrim($2), name), aliases = coalesce($3, aliases),
                        equipment = CASE WHEN $6::text IS NULL THEN coalesce($4, equipment)
                                         WHEN equipment = '' THEN btrim($6)
                                         ELSE equipment || E'\\n' || btrim($6) END,
                        notes = CASE WHEN $7::text IS NULL THEN coalesce($5, notes)
                                     WHEN notes = '' THEN btrim($7)
                                     ELSE notes || E'\\n' || btrim($7) END
         WHERE id = $1",
    )
    .bind(id)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.equipment)
    .bind(&a.notes)
    .bind(&a.equipment_append)
    .bind(&a.notes_append)
    .execute(&mut *tx)
    .await?;
    let v = gym_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

async fn gym_json(tx: &mut Tx, owner: &str, id: i64) -> AppResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT json_build_object(
            'id', g.id, 'name', g.name, 'aliases', g.aliases,
            'equipment', NULLIF(g.equipment, ''), 'notes', NULLIF(g.notes, ''),
            'last_workout_at', (SELECT max(started_at) FROM workout WHERE gym_id = g.id))
         FROM gym g WHERE g.id = $1 AND g.owner_sub = $2",
    )
    .bind(id)
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?)
}

#[derive(Deserialize, JsonSchema)]
pub struct GymRef {
    /// Gym name, alias, or id.
    pub gym: String,
}

pub async fn gym_get(app: &App, owner: &str, a: GymRef) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    let v = gym_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

pub async fn gym_list(app: &App, owner: &str) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let v: Value = sqlx::query_scalar(
        "SELECT coalesce(json_agg(json_build_object(
                    'id', g.id, 'name', g.name, 'aliases', g.aliases,
                    'last_workout_at', (SELECT max(started_at) FROM workout WHERE gym_id = g.id)) ORDER BY g.name), '[]'::json)
         FROM gym g WHERE g.owner_sub = $1",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

// ---------- exercises ----------

#[derive(Deserialize, JsonSchema, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum ExerciseKind {
    /// Reps and weight (default).
    Strength,
    /// Duration, distance and intensity: bike, treadmill, rower.
    Cardio,
}

impl ExerciseKind {
    fn as_str(self) -> &'static str {
        match self {
            ExerciseKind::Strength => "strength",
            ExerciseKind::Cardio => "cardio",
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ExerciseCreateArgs {
    /// Specific enough to compare weights: "Dumbbell bench press", not "bench".
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub description: Option<String>,
    /// Primary (target) muscles. From: chest, lats, upper_back, lower_back, front_delts, side_delts, rear_delts,
    /// biceps, triceps, forearms, core, glutes, quads, hamstrings, adductors, abductors, calves, neck.
    #[serde(default)]
    pub muscle_groups: Vec<String>,
    /// Muscles that assist (bench press: triceps, front_delts). Same list, no overlap with muscle_groups.
    #[serde(default)]
    pub secondary_muscle_groups: Vec<String>,
    /// Free text: what it needs ("dumbbells + flat bench"). Empty = bodyweight.
    pub equipment: Option<String>,
    /// Default strength.
    pub kind: Option<ExerciseKind>,
}

pub const EXERCISE_JSON: &str = "json_build_object('id', e.id, 'name', e.name, 'aliases', e.aliases, 'kind', e.kind,
    'description', NULLIF(e.description, ''), 'muscle_groups', e.muscle_groups,
    'secondary_muscle_groups', e.secondary_muscle_groups, 'equipment', NULLIF(e.equipment, ''))";

async fn exercise_json(tx: &mut Tx, owner: &str, id: i64) -> AppResult<Value> {
    Ok(sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT {EXERCISE_JSON} FROM exercise e WHERE e.id = $1 AND e.owner_sub = $2"
    )))
    .bind(id)
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?)
}

pub async fn exercise_create(app: &App, owner: &str, a: ExerciseCreateArgs) -> AppResult<Value> {
    check_muscle_groups(&a.muscle_groups, &a.secondary_muscle_groups)?;
    let mut tx = db::begin(app, owner).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO exercise (owner_sub, name, aliases, description, muscle_groups, equipment, kind,
                               secondary_muscle_groups)
         VALUES ($1, btrim($2), $3, coalesce($4, ''), $5, coalesce($6, ''), coalesce($7, 'strength'), $8) RETURNING id",
    )
    .bind(owner)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.description)
    .bind(&a.muscle_groups)
    .bind(&a.equipment)
    .bind(a.kind.map(ExerciseKind::as_str))
    .bind(&a.secondary_muscle_groups)
    .fetch_one(&mut *tx)
    .await?;
    let v = exercise_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct ExerciseUpdateArgs {
    /// Exercise name, alias, or id.
    pub exercise: String,
    pub name: Option<String>,
    /// Replaces the list.
    pub aliases: Option<Vec<String>>,
    pub description: Option<String>,
    /// Primary muscles; replaces the list.
    pub muscle_groups: Option<Vec<String>>,
    /// Replaces the list.
    pub secondary_muscle_groups: Option<Vec<String>>,
    /// Replaces the equipment text.
    pub equipment: Option<String>,
    pub kind: Option<ExerciseKind>,
}

pub async fn exercise_update(app: &App, owner: &str, a: ExerciseUpdateArgs) -> AppResult<Value> {
    // Overlap with the stored other list is caught by the table's CHECK.
    check_muscle_groups(
        a.muscle_groups.as_deref().unwrap_or_default(),
        a.secondary_muscle_groups.as_deref().unwrap_or_default(),
    )?;
    let mut tx = db::begin(app, owner).await?;
    let id = resolve::id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    sqlx::query(
        "UPDATE exercise SET name = coalesce(btrim($2), name), aliases = coalesce($3, aliases),
                             description = coalesce($4, description), muscle_groups = coalesce($5, muscle_groups),
                             equipment = coalesce($6, equipment), kind = coalesce($7, kind),
                             secondary_muscle_groups = coalesce($8, secondary_muscle_groups)
         WHERE id = $1",
    )
    .bind(id)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.description)
    .bind(&a.muscle_groups)
    .bind(&a.equipment)
    .bind(a.kind.map(ExerciseKind::as_str))
    .bind(&a.secondary_muscle_groups)
    .execute(&mut *tx)
    .await?;
    let v = exercise_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct ExerciseSearchArgs {
    /// Words to match in the name or aliases (typos are tolerated). Omit to list all.
    pub query: Option<String>,
    /// Matches primary muscles. Coarse names work too: back, shoulders, legs, arms.
    pub muscle_group: Option<String>,
}

pub async fn exercise_search(app: &App, owner: &str, a: ExerciseSearchArgs) -> AppResult<Value> {
    let groups = expand_opt(&a.muscle_group)?;
    let mut tx = db::begin_read(app, owner).await?;
    // Narrow by name with the resolver's scoring so "benchpres" still finds things.
    let ids: Option<Vec<i64>> = match &a.query {
        Some(q) => Some(match resolve::lookup(&mut tx, owner, Kind::Exercise, q).await? {
            resolve::Outcome::Hit(id) => vec![id],
            resolve::Outcome::Ambiguous(c) | resolve::Outcome::NotFound(c) => c.into_iter().map(|(id, _)| id).collect(),
        }),
        None => None,
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg({EXERCISE_JSON} ORDER BY e.name), '[]'::json)
         FROM exercise e
         WHERE e.owner_sub = $1
           AND ($2::bigint[] IS NULL OR e.id = ANY($2))
           AND ($3::text[] IS NULL OR e.muscle_groups && $3)"
    )))
    .bind(owner)
    .bind(&ids)
    .bind(&groups)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}
