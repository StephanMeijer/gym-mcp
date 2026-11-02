//! Settings, gyms, equipment inventory and exercises.

use schemars::JsonSchema;
use serde::Deserialize;
// Dynamic SQL only interpolates compile-time constants and enum-mapped literals; user input is always bound.
use serde_json::Value;
use sqlx::AssertSqlSafe;

use crate::db::{self, App, AppResult, Tx, user_err};
use crate::resolve::{self, Kind};

pub const MUSCLE_GROUPS: &[&str] = &[
    "chest",
    "back",
    "shoulders",
    "biceps",
    "triceps",
    "forearms",
    "core",
    "glutes",
    "quads",
    "hamstrings",
    "calves",
    "cardio",
];

/// Per exercise at one gym: availability ∈ {available, unverified, unavailable} plus which
/// required equipment is unknown or absent. No gym_equipment row = unknown, unless the gym's
/// inventory is marked complete. Binds: $1 owner, $2 gym id.
pub const AVAILABILITY_CTE: &str = "
    avail AS (
        SELECT e.id AS exercise_id,
               CASE WHEN bool_or(ge.status = 'absent')
                         OR (g.inventory_complete AND bool_or(ee.equipment_id IS NOT NULL AND ge.status IS NULL))
                    THEN 'unavailable'
                    WHEN bool_or(ee.equipment_id IS NOT NULL AND ge.status IS NULL) THEN 'unverified'
                    ELSE 'available' END AS availability,
               coalesce(array_agg(eq.name ORDER BY eq.name) FILTER (WHERE ee.equipment_id IS NOT NULL), '{}') AS equipment,
               coalesce(array_agg(eq.name ORDER BY eq.name) FILTER (WHERE ee.equipment_id IS NOT NULL AND ge.status IS NULL), '{}') AS unknown_equipment,
               coalesce(array_agg(eq.name ORDER BY eq.name) FILTER (WHERE ge.status = 'absent'), '{}') AS absent_equipment
        FROM exercise e
        JOIN gym g ON g.id = $2 AND g.owner_sub = $1
        LEFT JOIN exercise_equipment ee ON ee.exercise_id = e.id
        LEFT JOIN equipment eq ON eq.id = ee.equipment_id
        LEFT JOIN gym_equipment ge ON ge.gym_id = g.id AND ge.equipment_id = ee.equipment_id
        WHERE e.owner_sub = $1
        GROUP BY e.id, g.inventory_complete
    )";

fn check_muscle_groups(groups: &[String]) -> AppResult<()> {
    match groups.iter().find(|g| !MUSCLE_GROUPS.contains(&g.as_str())) {
        Some(bad) => Err(user_err(format!(
            "unknown muscle group '{bad}'; use one of {}",
            MUSCLE_GROUPS.join(", ")
        ))),
        None => Ok(()),
    }
}

// ---------- settings ----------

pub async fn settings_get(app: &App, owner: &str) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let v: Value = sqlx::query_scalar(
        "SELECT json_build_object('timezone', current_setting('TimeZone'), 'now', now(), 'today', current_date,
                                  'goal', (SELECT goal FROM user_settings WHERE owner_sub = $1))",
    )
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
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
}

pub async fn settings_update(app: &App, owner: &str, a: SettingsArgs) -> AppResult<Value> {
    if a.timezone.is_none() && a.goal.is_none() {
        return Err(user_err("pass timezone and/or goal"));
    }
    let mut tx = db::begin(app, owner).await?;
    if let Some(tz) = &a.timezone {
        // Postgres validates the name against its own tz database (22023 → user error).
        sqlx::query("SELECT set_config('TimeZone', $1, true)")
            .bind(tz)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "UPDATE user_settings SET timezone = coalesce($2, timezone), goal = coalesce($3, goal) WHERE owner_sub = $1",
    )
    .bind(owner)
    .bind(&a.timezone)
    .bind(a.goal.map(Goal::as_str))
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
    pub notes: Option<String>,
    /// True once every piece of equipment there is recorded: unknown then counts as absent.
    #[serde(default)]
    pub inventory_complete: bool,
}

pub async fn gym_create(app: &App, owner: &str, a: GymCreateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO gym (owner_sub, name, aliases, notes, inventory_complete) VALUES ($1, btrim($2), $3, coalesce($4, ''), $5) RETURNING id",
    )
    .bind(owner)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.notes)
    .bind(a.inventory_complete)
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
    pub notes: Option<String>,
    pub inventory_complete: Option<bool>,
}

pub async fn gym_update(app: &App, owner: &str, a: GymUpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    sqlx::query(
        "UPDATE gym SET name = coalesce(btrim($2), name), aliases = coalesce($3, aliases), notes = coalesce($4, notes),
                        inventory_complete = coalesce($5, inventory_complete)
         WHERE id = $1",
    )
    .bind(id)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.notes)
    .bind(a.inventory_complete)
    .execute(&mut *tx)
    .await?;
    let v = gym_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

async fn gym_json(tx: &mut Tx, owner: &str, id: i64) -> AppResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT json_build_object(
            'id', g.id, 'name', g.name, 'aliases', g.aliases, 'notes', NULLIF(g.notes, ''),
            'inventory_complete', g.inventory_complete,
            'equipment_present', coalesce((
                SELECT json_agg(json_build_object(
                    'name', eq.name, 'category', eq.category, 'description', NULLIF(eq.description, ''),
                    'quantity', ge.quantity,
                    'available_weights_kg', ge.available_weights_kg, 'max_load_kg', ge.max_load_kg,
                    'notes', NULLIF(ge.notes, ''), 'verified_at', ge.verified_at) ORDER BY eq.category, eq.name)
                FROM gym_equipment ge JOIN equipment eq ON eq.id = ge.equipment_id
                WHERE ge.gym_id = g.id AND ge.status = 'present'), '[]'::json),
            'equipment_absent', coalesce((
                SELECT json_agg(eq.name ORDER BY eq.name)
                FROM gym_equipment ge JOIN equipment eq ON eq.id = ge.equipment_id
                WHERE ge.gym_id = g.id AND ge.status = 'absent'), '[]'::json),
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

// ---------- equipment ----------

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Machine,
    FreeWeight,
    RackBench,
    Cardio,
    Accessory,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Category::Machine => "machine",
            Category::FreeWeight => "free_weight",
            Category::RackBench => "rack_bench",
            Category::Cardio => "cardio",
            Category::Accessory => "accessory",
        }
    }
}

#[derive(Deserialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Present,
    Absent,
}

/// Finds equipment by name (fuzzy) or creates it; given fields overwrite. Ambiguous names are an error.
async fn upsert_equipment(
    tx: &mut Tx,
    owner: &str,
    name: &str,
    category: Option<Category>,
    description: Option<&str>,
) -> AppResult<i64> {
    let category = category.map(Category::as_str);
    if let Some(id) = resolve::existing_id(tx, owner, Kind::Equipment, name).await? {
        sqlx::query(
            "UPDATE equipment SET category = coalesce($2, category), description = coalesce($3, description) WHERE id = $1",
        )
        .bind(id)
        .bind(category)
        .bind(description)
        .execute(&mut **tx)
        .await?;
        return Ok(id);
    }
    Ok(sqlx::query_scalar(
        "INSERT INTO equipment (owner_sub, name, category, description)
         VALUES ($1, btrim($2), coalesce($3, 'accessory'), coalesce($4, '')) RETURNING id",
    )
    .bind(owner)
    .bind(name)
    .bind(category)
    .bind(description)
    .fetch_one(&mut **tx)
    .await?)
}

#[derive(Deserialize, JsonSchema)]
pub struct EquipmentItem {
    /// "leg press", "dumbbells", "mat"… Matched to existing equipment, created if new.
    pub name: String,
    /// Default present. Use absent when you've confirmed the gym does NOT have it.
    pub status: Option<Presence>,
    pub category: Option<Category>,
    /// What this kind of equipment is, the same at every gym ("45° plate-loaded leg press").
    pub description: Option<String>,
    pub quantity: Option<i32>,
    /// For fixed-weight implements (dumbbells, kettlebells): the weights on the rack, e.g. [2, 4, 6, …, 40].
    /// Not plates: logged barbell weights are totals and would be flagged.
    pub available_weights_kg: Option<Vec<f64>>,
    /// For machines: the heaviest setting.
    pub max_load_kg: Option<f64>,
    /// About it at THIS gym ("2×, upstairs by the window").
    pub notes: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
pub struct GymEquipmentSetArgs {
    /// Gym name, alias, or id.
    pub gym: String,
    pub items: Vec<EquipmentItem>,
}

pub async fn gym_equipment_set(app: &App, owner: &str, a: GymEquipmentSetArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let gym_id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    for it in &a.items {
        let eq_id = upsert_equipment(&mut tx, owner, &it.name, it.category, it.description.as_deref()).await?;
        let status = if it.status == Some(Presence::Absent) {
            "absent"
        } else {
            "present"
        };
        sqlx::query(
            "INSERT INTO gym_equipment (gym_id, equipment_id, status, quantity, available_weights_kg, max_load_kg, notes)
             VALUES ($1, $2, $3, $4, $5, $6, coalesce($7, ''))
             ON CONFLICT (gym_id, equipment_id) DO UPDATE SET
                status = EXCLUDED.status,
                quantity = coalesce($4, gym_equipment.quantity),
                available_weights_kg = coalesce($5, gym_equipment.available_weights_kg),
                max_load_kg = coalesce($6, gym_equipment.max_load_kg),
                notes = coalesce($7, gym_equipment.notes),
                verified_at = now()",
        )
        .bind(gym_id)
        .bind(eq_id)
        .bind(status)
        .bind(it.quantity)
        .bind(&it.available_weights_kg)
        .bind(it.max_load_kg)
        .bind(&it.notes)
        .execute(&mut *tx)
        .await?;
    }
    let v = gym_json(&mut tx, owner, gym_id).await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct GymEquipmentForgetArgs {
    pub gym: String,
    /// Equipment to set back to "unknown" at this gym.
    pub names: Vec<String>,
}

pub async fn gym_equipment_forget(app: &App, owner: &str, a: GymEquipmentForgetArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let gym_id = resolve::id(&mut tx, owner, Kind::Gym, &a.gym).await?;
    for n in &a.names {
        let eq_id = resolve::id(&mut tx, owner, Kind::Equipment, n).await?;
        sqlx::query("DELETE FROM gym_equipment WHERE gym_id = $1 AND equipment_id = $2")
            .bind(gym_id)
            .bind(eq_id)
            .execute(&mut *tx)
            .await?;
    }
    let v = gym_json(&mut tx, owner, gym_id).await?;
    tx.commit().await?;
    Ok(v)
}

const EQUIPMENT_JSON: &str = "json_build_object('id', id, 'name', name, 'aliases', aliases, 'category', category,
                                               'description', NULLIF(description, ''))";

pub async fn equipment_list(app: &App, owner: &str) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "SELECT coalesce(json_agg({EQUIPMENT_JSON} ORDER BY category, name), '[]'::json)
         FROM equipment WHERE owner_sub = $1"
    )))
    .bind(owner)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct EquipmentUpdateArgs {
    /// Equipment name, alias, or id.
    pub equipment: String,
    pub name: Option<String>,
    /// Replaces the alias list.
    pub aliases: Option<Vec<String>>,
    pub category: Option<Category>,
    /// What this kind of equipment is, the same at every gym. Gym-specific details go in gym_equipment_set notes.
    pub description: Option<String>,
}

pub async fn equipment_update(app: &App, owner: &str, a: EquipmentUpdateArgs) -> AppResult<Value> {
    let mut tx = db::begin(app, owner).await?;
    let id = resolve::id(&mut tx, owner, Kind::Equipment, &a.equipment).await?;
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "UPDATE equipment SET name = coalesce(btrim($2), name), aliases = coalesce($3, aliases),
                              category = coalesce($4, category), description = coalesce($5, description)
         WHERE id = $1 RETURNING {EQUIPMENT_JSON}"
    )))
    .bind(id)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(a.category.map(Category::as_str))
    .bind(&a.description)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}

// ---------- exercises ----------

#[derive(Deserialize, JsonSchema)]
pub struct ExerciseCreateArgs {
    /// Specific enough to compare weights: "Dumbbell bench press", not "bench".
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub description: Option<String>,
    /// From: chest, back, shoulders, biceps, triceps, forearms, core, glutes, quads, hamstrings, calves, cardio.
    #[serde(default)]
    pub muscle_groups: Vec<String>,
    /// ALL equipment it needs (names; created if new). Empty = bodyweight.
    #[serde(default)]
    pub equipment: Vec<String>,
}

async fn set_exercise_equipment(tx: &mut Tx, owner: &str, exercise_id: i64, names: &[String]) -> AppResult<()> {
    sqlx::query("DELETE FROM exercise_equipment WHERE exercise_id = $1")
        .bind(exercise_id)
        .execute(&mut **tx)
        .await?;
    for n in names {
        let eq_id = upsert_equipment(tx, owner, n, None, None).await?;
        sqlx::query("INSERT INTO exercise_equipment VALUES ($1, $2) ON CONFLICT DO NOTHING")
            .bind(exercise_id)
            .bind(eq_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn exercise_json(tx: &mut Tx, owner: &str, id: i64) -> AppResult<Value> {
    Ok(sqlx::query_scalar(
        "SELECT json_build_object('id', e.id, 'name', e.name, 'aliases', e.aliases, 'description', NULLIF(e.description, ''),
                    'muscle_groups', e.muscle_groups,
                    'equipment', coalesce((SELECT json_agg(eq.name ORDER BY eq.name) FROM exercise_equipment ee
                                           JOIN equipment eq ON eq.id = ee.equipment_id WHERE ee.exercise_id = e.id), '[]'::json))
         FROM exercise e WHERE e.id = $1 AND e.owner_sub = $2",
    )
    .bind(id)
    .bind(owner)
    .fetch_one(&mut **tx)
    .await?)
}

pub async fn exercise_create(app: &App, owner: &str, a: ExerciseCreateArgs) -> AppResult<Value> {
    check_muscle_groups(&a.muscle_groups)?;
    let mut tx = db::begin(app, owner).await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO exercise (owner_sub, name, aliases, description, muscle_groups) VALUES ($1, btrim($2), $3, coalesce($4, ''), $5) RETURNING id",
    )
    .bind(owner)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.description)
    .bind(&a.muscle_groups)
    .fetch_one(&mut *tx)
    .await?;
    set_exercise_equipment(&mut tx, owner, id, &a.equipment).await?;
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
    /// Replaces the list.
    pub muscle_groups: Option<Vec<String>>,
    /// Replaces the list.
    pub equipment: Option<Vec<String>>,
}

pub async fn exercise_update(app: &App, owner: &str, a: ExerciseUpdateArgs) -> AppResult<Value> {
    if let Some(g) = &a.muscle_groups {
        check_muscle_groups(g)?;
    }
    let mut tx = db::begin(app, owner).await?;
    let id = resolve::id(&mut tx, owner, Kind::Exercise, &a.exercise).await?;
    sqlx::query(
        "UPDATE exercise SET name = coalesce(btrim($2), name), aliases = coalesce($3, aliases),
                             description = coalesce($4, description), muscle_groups = coalesce($5, muscle_groups)
         WHERE id = $1",
    )
    .bind(id)
    .bind(&a.name)
    .bind(&a.aliases)
    .bind(&a.description)
    .bind(&a.muscle_groups)
    .execute(&mut *tx)
    .await?;
    if let Some(eq) = &a.equipment {
        set_exercise_equipment(&mut tx, owner, id, eq).await?;
    }
    let v = exercise_json(&mut tx, owner, id).await?;
    tx.commit().await?;
    Ok(v)
}

#[derive(Deserialize, JsonSchema)]
pub struct ExerciseSearchArgs {
    /// Words to match in the name or aliases (typos are tolerated). Omit to list all.
    pub query: Option<String>,
    pub muscle_group: Option<String>,
    /// With a gym, each exercise gets availability: available / unverified (some equipment unknown there) / unavailable.
    pub gym: Option<String>,
    /// Include exercises whose equipment is confirmed absent at the gym. Default false.
    #[serde(default)]
    pub include_unavailable: bool,
}

pub async fn exercise_search(app: &App, owner: &str, a: ExerciseSearchArgs) -> AppResult<Value> {
    let mut tx = db::begin_read(app, owner).await?;
    // Narrow by name with the resolver's scoring so "benchpres" still finds things.
    let ids: Option<Vec<i64>> = match &a.query {
        Some(q) => Some(match resolve::lookup(&mut tx, owner, Kind::Exercise, q).await? {
            resolve::Outcome::Hit(id) => vec![id],
            resolve::Outcome::Ambiguous(c) | resolve::Outcome::NotFound(c) => c.into_iter().map(|(id, _)| id).collect(),
        }),
        None => None,
    };
    let gym_id = match &a.gym {
        Some(g) => resolve::id(&mut tx, owner, Kind::Gym, g).await?,
        None => -1,
    };
    let v: Value = sqlx::query_scalar(AssertSqlSafe(format!(
        "WITH {AVAILABILITY_CTE}
         SELECT coalesce(json_agg(json_build_object(
                    'id', e.id, 'name', e.name, 'aliases', e.aliases, 'description', NULLIF(e.description, ''),
                    'muscle_groups', e.muscle_groups,
                    'equipment', coalesce(av.equipment, (SELECT array_agg(eq.name ORDER BY eq.name) FROM exercise_equipment ee
                                                         JOIN equipment eq ON eq.id = ee.equipment_id WHERE ee.exercise_id = e.id)),
                    'availability', av.availability,
                    'unknown_equipment', NULLIF(av.unknown_equipment, '{{}}'),
                    'absent_equipment', NULLIF(av.absent_equipment, '{{}}'))
                ORDER BY CASE av.availability WHEN 'available' THEN 0 WHEN 'unverified' THEN 1 ELSE 2 END, e.name), '[]'::json)
         FROM exercise e
         LEFT JOIN avail av ON av.exercise_id = e.id
         WHERE e.owner_sub = $1
           AND ($3::bigint[] IS NULL OR e.id = ANY($3))
           AND ($4::text IS NULL OR $4 = ANY(e.muscle_groups))
           AND ($2 = -1 OR $5 OR av.availability <> 'unavailable')"
    )))
    .bind(owner)
    .bind(gym_id)
    .bind(&ids)
    .bind(&a.muscle_group)
    .bind(a.include_unavailable)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(v)
}
