//! MCP glue: each tool extracts the caller and its arguments and calls the domain function.

use axum::http::request::Parts;
use rmcp::handler::server::{tool::Extension, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::auth::Owner;
use crate::body_note::*;
use crate::db::{App, AppError, AppResult};
use crate::measurement::*;
use crate::search::*;
use crate::setup::*;
use crate::workout::*;

const INSTRUCTIONS: &str = "\
Gym logbook. Typical flow:
1. User arrives (\"I'm at the NDSM Trainmore\") → workout_start(gym). Same gym again just resumes. If the gym is unknown, ask, then gym_create.
2. User reports sets, often with a photo of the machine/weights (\"10/8/6 on 20kg\", \"12 on 13kg\") → identify the exercise, \
   exercise_search to find it; only if it truly does not exist, exercise_create with its muscle groups and the equipment it needs (free text). \
   Then set_add with one entry per set. Never invent ids; names and aliases are accepted everywhere.
3. Anything not done just now (\"forgot: 3x10 squats at 19:40\") → pass performed_at. A whole past session → workout_add_past, \
   then set_add with its workout_id. Corrections → set_update / set_delete (set_ids for the same fix on several sets at once). \
A whole session logged at the wrong time → workout_update with started_at and shift_sets true (moves its sets along). \
A vague time (\"around noon\", \"this morning\") → pass it with time_estimated true (set_add per set, workout_add_past); \
once the real time is known, update it, which clears the flag.
4. \"What next?\" → training_overview (and exercise_alternatives if something is busy). Recommend from that data. \
   Judge what's possible by comparing each exercise's equipment text with the gym's equipment text; if the gym's text \
   doesn't say, ask the user, then add what you learn to the gym with gym_update equipment_append.
5. When the user leaves → workout_finish. Stale workouts close automatically.
Gym equipment: one free-text field per gym. When the user shows or mentions equipment, add it as a new line with gym_update \
equipment_append (use equipment only to rewrite the whole text): what it is, how many, weight range or max load, where it is, and what's missing.
Muscle groups: muscle_groups = the target (primary) muscles, secondary_muscle_groups = the ones that assist (bench press: chest; secondary triceps, front_delts). Filters accept the coarse names back, shoulders, legs and arms; exercises store the specific ones.
Descriptions: when creating exercises, add a short description (what it looks like, how it's set up) so it can be \
recognized from a photo later.
Muscle balance: training_overview.muscle_groups has a row per muscle: muscle_group, days_since (primary only), sets_7d / sets_28d \
(weighted: primary 1, secondary 0.5; strength only), train_days_28d and avg_rpe_28d. Rule of thumb for hypertrophy: \
~10-20 sets per muscle per week, trained 2+ times a week; for strength, fewer and heavier sets. Point out muscles well \
below that or not trained in 14+ days; the judgement is yours.
For a specific period (\"last 10 days\", \"October\") use muscle_group_volume; for trends over time, progress.
Effort: when the user says how a set felt, record rpe (1-10; \"2 left in the tank\" = RPE 8, \"to failure\" = 10) and rest_s \
if mentioned. Ask once for their goal, their goals and plans in their own words (including any event and its date) and \
any injuries or limitations; save them with settings_update (goal, goals_notes, priority_muscle_groups, limitations). \
training_overview returns them: take goals_notes into account (taper as a named event gets close), favour \
priority_muscle_groups (aim for the upper end of their weekly sets), and never suggest exercises that conflict with \
limitations; pick from exercise_alternatives instead. Recommend progression from goal + last_session in \
training_overview: e.g. all sets at RPE <= 7 → add weight or reps. If a logged weight doesn't fit the gym's equipment text, mention it.
Set type: record warm-ups (set_type warmup), deliberately partial reps (partial) and sets cut short (interrupted); \
only working sets count towards volume, progress, personal records and last_session.
Weights: the number on the implement (per dumbbell/kettlebell); barbell = total incl. bar; machine = stack setting. Convert lb to kg.
One arm or leg at a time (single-arm row, Bulgarian split squat): set_add unilateral true, reps per side; it then counts \
once per side. Pass load_basis total only when the weight is for both sides together.
Machines: stack weights only compare within one brand. Pass set_add machine (brand/model, read off the photo when \
visible; reuse a name from machines_used in training_overview). Omitted, it reuses the last machine for that exercise \
at this gym; check machine_source in the result. Note in the gym's equipment text which brands it has \
(\"leg press: Technogym + Hammer Strength\"). last_session in training_overview is from the same machine.
Grip or variant (neutral grip, wide, seated, incline 30°): pass set_add variant instead of writing it in notes. \
Omitted, it reuses the last variant for that exercise at this gym (check variant_source); \"\" = standard. \
History, records and last_session are split per variant, like per machine.
Cardio: exercises with kind cardio. \"20 min bike, level 8, 7.5 km\" → one set with duration_s 1200, distance_m 7500, \
intensity \"level 8\". progress/personal_records/set_search report distance and pace (seconds per km). \
Read average/max heart rate, calories, average/max incline and average/max speed off the machine's screen or photo and pass \
them on the set (avg_hr, max_hr, calories, avg_incline_pct, max_incline_pct, avg_speed, max_speed; speed_unit mph when \
the screen shows mph), not in notes or intensity. progress charts avg_hr, avg_speed and calories.
Measurements: log body_fat (%) and blood pressure: \"128/82\" = two measurement_add calls, blood_pressure_systolic 128 \
and blood_pressure_diastolic 82. training_overview.measurements has the latest of each with its 28-day average. \
Don't diagnose blood pressure; if it's high, suggest seeing a doctor and take it into account like a limitation.
Body notes: when the user mentions pain, an ache or fatigue (\"lower back tired tonight\"), body_note_add with a \
location (a muscle group name where it fits, else free text like \"left knee\") and severity 1-10 if they give one or \
it's clear. training_overview.body_notes has the last 14 days: avoid loading a sore area, suggest lighter work or \
alternatives, and point out repeats (body_note_list for longer patterns). Don't diagnose; for sharp or lasting pain, \
suggest seeing a professional.
Times: answers are in the user's timezone; inputs may be RFC 3339 or local (\"2026-11-02 19:40\").
Photos stay with you; send only text.";

fn out(r: AppResult<Value>) -> Result<CallToolResult, ErrorData> {
    Ok(match r {
        Ok(v) => CallToolResult::success(vec![ContentBlock::text(v.to_string())]),
        Err(AppError::User(m)) => CallToolResult::error(vec![ContentBlock::text(m)]),
        Err(AppError::Db(e)) => {
            tracing::error!("database error: {e}");
            CallToolResult::error(vec![ContentBlock::text("internal error, try again")])
        }
    })
}

fn owner(parts: &Parts) -> Result<String, ErrorData> {
    parts
        .extensions
        .get::<Owner>()
        .map(|o| o.0.clone())
        .ok_or_else(|| ErrorData::internal_error("unauthenticated request reached a tool", None))
}

#[derive(Clone)]
pub struct GymServer {
    app: App,
}

impl GymServer {
    pub fn new(app: App) -> Self {
        Self { app }
    }
}

/// `call!(self, parts, f)` / `call!(self, parts, f, args)`: call a domain fn as the caller.
macro_rules! call {
    ($s:ident, $p:ident, $f:ident $(, $a:ident)?) => {{
        let o = owner(&$p.0)?;
        out($f(&$s.app, &o $(, $a.0)?).await)
    }};
}

type P = Extension<Parts>;
type R = Result<CallToolResult, ErrorData>;

#[tool_router]
impl GymServer {
    // ----- setup -----
    #[tool(
        description = "Show the user's settings (timezone, goal, goals notes, priority muscle groups, limitations), now and today.",
        annotations(title = "Show settings", read_only_hint = true, open_world_hint = false)
    )]
    async fn settings_get(&self, p: P) -> R {
        call!(self, p, settings_get)
    }

    #[tool(
        description = "Update settings; only the fields passed change. Timezone (IANA, defines today/weeks/months), goal, goals_notes (free text incl. events and dates), priority_muscle_groups, limitations. \"\" or [] clears a field.",
        annotations(
            title = "Update settings",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn settings_update(&self, p: P, a: Parameters<SettingsArgs>) -> R {
        call!(self, p, settings_update, a)
    }

    #[tool(
        description = "Create a gym.",
        annotations(
            title = "Create gym",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn gym_create(&self, p: P, a: Parameters<GymCreateArgs>) -> R {
        call!(self, p, gym_create, a)
    }

    #[tool(
        description = "Change a gym's name, aliases, equipment or notes (free text: replace it, or append a line).",
        annotations(
            title = "Update gym",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gym_update(&self, p: P, a: Parameters<GymUpdateArgs>) -> R {
        call!(self, p, gym_update, a)
    }

    #[tool(
        description = "A gym with its equipment text and notes.",
        annotations(title = "Show gym", read_only_hint = true, open_world_hint = false)
    )]
    async fn gym_get(&self, p: P, a: Parameters<GymRef>) -> R {
        call!(self, p, gym_get, a)
    }

    #[tool(
        description = "All gyms with their last workout date.",
        annotations(title = "List gyms", read_only_hint = true, open_world_hint = false)
    )]
    async fn gym_list(&self, p: P) -> R {
        call!(self, p, gym_list)
    }

    #[tool(
        description = "Create an exercise with its muscle groups and the equipment it needs (free text). Search first to avoid duplicates.",
        annotations(
            title = "Create exercise",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn exercise_create(&self, p: P, a: Parameters<ExerciseCreateArgs>) -> R {
        call!(self, p, exercise_create, a)
    }

    #[tool(
        description = "Change an exercise's name, aliases, description, muscle groups or equipment.",
        annotations(
            title = "Update exercise",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn exercise_update(&self, p: P, a: Parameters<ExerciseUpdateArgs>) -> R {
        call!(self, p, exercise_update, a)
    }

    #[tool(
        description = "Find exercises by name (typos ok) and/or muscle group.",
        annotations(title = "Find exercises", read_only_hint = true, open_world_hint = false)
    )]
    async fn exercise_search(&self, p: P, a: Parameters<ExerciseSearchArgs>) -> R {
        call!(self, p, exercise_search, a)
    }

    // ----- workout -----
    #[tool(
        description = "Start a workout at a gym, or resume the open one if it's the same gym.",
        annotations(
            title = "Start or resume workout",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workout_start(&self, p: P, a: Parameters<StartArgs>) -> R {
        call!(self, p, start, a)
    }

    #[tool(
        description = "Record a workout that already happened (created finished). Then set_add with its workout_id and performed_at.",
        annotations(
            title = "Add past workout",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn workout_add_past(&self, p: P, a: Parameters<AddPastArgs>) -> R {
        call!(self, p, add_past, a)
    }

    #[tool(
        description = "Finish the open workout.",
        annotations(
            title = "Finish workout",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workout_finish(&self, p: P, a: Parameters<FinishArgs>) -> R {
        call!(self, p, finish, a)
    }

    #[tool(
        description = "Correct a workout's start/finish time, gym or notes. shift_sets moves its sets along with the start time.",
        annotations(
            title = "Correct workout",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn workout_update(&self, p: P, a: Parameters<UpdateArgs>) -> R {
        call!(self, p, update, a)
    }

    #[tool(
        description = "The open workout with all its sets, grouped by exercise.",
        annotations(title = "Show current workout", read_only_hint = true, open_world_hint = false)
    )]
    async fn workout_current(&self, p: P) -> R {
        call!(self, p, current)
    }

    #[tool(
        description = "Past workouts with their sets, filterable by gym, exercise and date.",
        annotations(title = "List workouts", read_only_hint = true, open_world_hint = false)
    )]
    async fn workout_list(&self, p: P, a: Parameters<WorkoutListArgs>) -> R {
        call!(self, p, workout_list, a)
    }

    #[tool(
        description = "Log one or more sets of one exercise (one entry per set) to the open workout.",
        annotations(
            title = "Log sets",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn set_add(&self, p: P, a: Parameters<SetAddArgs>) -> R {
        call!(self, p, set_add, a)
    }

    #[tool(
        description = "Correct one or more logged sets (weight, reps, time, exercise, notes). Pass set_ids to apply the same change to several sets at once; all or nothing. Returns the workout for set_id, {\"workouts\": [...]} for set_ids. performed_at takes a single set.",
        annotations(
            title = "Correct set",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_update(&self, p: P, a: Parameters<SetUpdateArgs>) -> R {
        call!(self, p, set_update, a)
    }

    #[tool(
        description = "Delete one or more logged sets (set_ids for several at once; all or nothing). Returns the workout for set_id, {\"workouts\": [...]} for set_ids.",
        annotations(
            title = "Delete set",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn set_delete(&self, p: P, a: Parameters<SetDeleteArgs>) -> R {
        call!(self, p, set_delete, a)
    }

    // ----- measurements -----
    #[tool(
        description = "Log a body measurement (one value): body fat % or one side of blood pressure; optionally at a past measured_at.",
        annotations(
            title = "Log measurement",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn measurement_add(&self, p: P, a: Parameters<MeasurementAddArgs>) -> R {
        call!(self, p, measurement_add, a)
    }

    #[tool(
        description = "Body measurements over time, newest first, optionally within from-to.",
        annotations(title = "List measurements", read_only_hint = true, open_world_hint = false)
    )]
    async fn measurement_list(&self, p: P, a: Parameters<MeasurementListArgs>) -> R {
        call!(self, p, measurement_list, a)
    }

    #[tool(
        description = "Correct a measurement's value, time or notes.",
        annotations(
            title = "Correct measurement",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn measurement_update(&self, p: P, a: Parameters<MeasurementUpdateArgs>) -> R {
        call!(self, p, measurement_update, a)
    }

    #[tool(
        description = "Delete a measurement.",
        annotations(
            title = "Delete measurement",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn measurement_delete(&self, p: P, a: Parameters<MeasurementDeleteArgs>) -> R {
        call!(self, p, measurement_delete, a)
    }

    // ----- body notes -----
    #[tool(
        description = "Log an ache, pain or fatigue: where, how bad (1-10), when, and optionally the workout it came up in.",
        annotations(
            title = "Log body note",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    async fn body_note_add(&self, p: P, a: Parameters<BodyNoteAddArgs>) -> R {
        call!(self, p, body_note_add, a)
    }

    #[tool(
        description = "Body notes over time, newest first, optionally by location and within from-to.",
        annotations(title = "List body notes", read_only_hint = true, open_world_hint = false)
    )]
    async fn body_note_list(&self, p: P, a: Parameters<BodyNoteListArgs>) -> R {
        call!(self, p, body_note_list, a)
    }

    #[tool(
        description = "Correct a body note's location, severity, time, notes or workout.",
        annotations(
            title = "Correct body note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn body_note_update(&self, p: P, a: Parameters<BodyNoteUpdateArgs>) -> R {
        call!(self, p, body_note_update, a)
    }

    #[tool(
        description = "Delete a body note.",
        annotations(
            title = "Delete body note",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn body_note_delete(&self, p: P, a: Parameters<BodyNoteDeleteArgs>) -> R {
        call!(self, p, body_note_delete, a)
    }

    // ----- search & analysis -----
    #[tool(
        description = "Everything needed to recommend what to do next at a gym: its equipment text, every exercise with its equipment text and last session, per-muscle recency, weekly/4-week weighted sets, training days and RPE, today's work, the goal, the latest of every measurement with its 28-day average, and body notes from the last 14 days.",
        annotations(title = "Training overview", read_only_hint = true, open_world_hint = false)
    )]
    async fn training_overview(&self, p: P, a: Parameters<OverviewArgs>) -> R {
        call!(self, p, training_overview, a)
    }

    #[tool(
        description = "One row per muscle group over a period (default the last 7 days, or days / from-to): weighted sets (primary 1, secondary 0.5), sets per week, training days, average RPE, days since, and the exercises behind it.",
        annotations(
            title = "Muscle volume over a period",
            read_only_hint = true,
            open_world_hint = false
        )
    )]
    async fn muscle_group_volume(&self, p: P, a: Parameters<MuscleVolumeArgs>) -> R {
        call!(self, p, muscle_group_volume, a)
    }

    #[tool(
        description = "Free-text search across gym and exercise names, equipment texts, descriptions and notes.",
        annotations(title = "Search", read_only_hint = true, open_world_hint = false)
    )]
    async fn search(&self, p: P, a: Parameters<SearchArgs>) -> R {
        call!(self, p, search, a)
    }

    #[tool(
        description = "Find sets by exercise, muscle group, equipment, gym, date range, weight and reps; sort by date, weight, reps, volume, estimated 1RM, or for cardio distance or pace.",
        annotations(title = "Find sets", read_only_hint = true, open_world_hint = false)
    )]
    async fn set_search(&self, p: P, a: Parameters<SetSearchArgs>) -> R {
        call!(self, p, set_search, a)
    }

    #[tool(
        description = "Time series per day/week/month of top weight, estimated 1RM, volume, sets or reps, or for cardio distance, duration, pace, average heart rate, average speed or calories, for an exercise or a muscle group.",
        annotations(title = "Progress over time", read_only_hint = true, open_world_hint = false)
    )]
    async fn progress(&self, p: P, a: Parameters<ProgressArgs>) -> R {
        call!(self, p, progress, a)
    }

    #[tool(
        description = "Personal records per exercise: heaviest set, best estimated 1RM, max reps at each weight, best volume workout; for cardio longest distance, best pace and longest duration.",
        annotations(title = "Personal records", read_only_hint = true, open_world_hint = false)
    )]
    async fn personal_records(&self, p: P, a: Parameters<RecordsArgs>) -> R {
        call!(self, p, personal_records, a)
    }

    #[tool(
        description = "Exercises that train the same muscles (shared primary first, then secondary), with the gym's equipment text (default: the current gym) to judge what's possible.",
        annotations(title = "Alternative exercises", read_only_hint = true, open_world_hint = false)
    )]
    async fn exercise_alternatives(&self, p: P, a: Parameters<AlternativesArgs>) -> R {
        call!(self, p, exercise_alternatives, a)
    }
}

#[tool_handler]
impl ServerHandler for GymServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("gym-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    use super::GymServer;

    #[test]
    fn every_tool_is_annotated_and_closed_world() {
        let tools = GymServer::tool_router().list_all();
        assert_eq!(tools.len(), 33);
        for t in &tools {
            let a = t
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} has no annotations", t.name));
            assert!(a.title.is_some(), "{} has no title", t.name);
            assert_eq!(a.open_world_hint, Some(false), "{}", t.name);
            if a.read_only_hint != Some(true) {
                assert!(
                    a.destructive_hint.is_some() && a.idempotent_hint.is_some(),
                    "{}",
                    t.name
                );
            }
        }
        let hint = |n: &str| tools.iter().find(|t| t.name == n).unwrap().annotations.clone().unwrap();
        assert_eq!(hint("training_overview").read_only_hint, Some(true));
        assert_eq!(hint("workout_current").read_only_hint, Some(true));
        assert_eq!(hint("set_delete").destructive_hint, Some(true));
        assert_eq!(
            hint("body_note_update").destructive_hint,
            Some(true),
            "it can clear severity and the workout link"
        );
        assert_eq!(hint("set_add").idempotent_hint, Some(false));
        assert_eq!(hint("workout_start").idempotent_hint, Some(true));
    }
}
