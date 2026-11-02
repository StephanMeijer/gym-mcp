//! MCP glue: each tool extracts the caller and its arguments and calls the domain function.

use axum::http::request::Parts;
use rmcp::handler::server::{tool::Extension, wrapper::Parameters};
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::auth::Owner;
use crate::db::{App, AppError, AppResult};
use crate::search::*;
use crate::setup::*;
use crate::workout::*;

const INSTRUCTIONS: &str = "\
Gym logbook. Typical flow:
1. User arrives (\"I'm at the NDSM Trainmore\") → workout_start(gym). Same gym again just resumes. If the gym is unknown, ask, then gym_create.
2. User reports sets, often with a photo of the machine/weights (\"10/8/6 on 20kg\", \"12 on 13kg\") → identify the exercise, \
   exercise_search to find it; only if it truly does not exist, exercise_create with its muscle groups and ALL equipment it needs. \
   Then set_add with one entry per set. Never invent ids; names and aliases are accepted everywhere.
3. Anything not done just now (\"forgot: 3x10 squats at 19:40\") → pass performed_at. A whole past session → workout_add_past, \
   then set_add with its workout_id. Corrections → set_update / set_delete.
4. \"What next?\" → training_overview (and exercise_alternatives if something is busy). Recommend from that data. \
   Exercises marked 'unverified' need equipment we don't know the gym has: ask, then record it with gym_equipment_set \
   (status 'absent' if the gym doesn't have it).
5. When the user leaves → workout_finish. Stale workouts close automatically.
Descriptions: when creating exercises or equipment, add a short description (what it looks like, how it's set up) \
so it can be recognized from a photo later; equipment description = the kind, gym_equipment_set notes = that gym's unit.
Weights: the number on the implement (per dumbbell/kettlebell); barbell = total incl. bar; machine = stack setting. Convert lb to kg.
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
        description = "Show the user's timezone, now and today.",
        annotations(title = "Show settings", read_only_hint = true, open_world_hint = false)
    )]
    async fn settings_get(&self, p: P) -> R {
        call!(self, p, settings_get)
    }

    #[tool(
        description = "Set the user's IANA timezone (\"Europe/Amsterdam\"); it defines today, weeks and months.",
        annotations(
            title = "Set timezone",
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
        description = "Change a gym's name, aliases, notes or inventory_complete.",
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
        description = "A gym with its equipment: present (with weights/loads) and confirmed absent. Unlisted = unknown.",
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
        description = "Record what a gym has (or confirm it lacks): leg press, dumbbells 2-40kg, mats… Creates equipment kinds as needed; updates existing entries.",
        annotations(
            title = "Record gym equipment",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gym_equipment_set(&self, p: P, a: Parameters<GymEquipmentSetArgs>) -> R {
        call!(self, p, gym_equipment_set, a)
    }

    #[tool(
        description = "Set equipment at a gym back to unknown.",
        annotations(
            title = "Forget gym equipment",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn gym_equipment_forget(&self, p: P, a: Parameters<GymEquipmentForgetArgs>) -> R {
        call!(self, p, gym_equipment_forget, a)
    }

    #[tool(
        description = "Change an equipment kind's name, aliases, category or description (applies at every gym).",
        annotations(
            title = "Update equipment",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn equipment_update(&self, p: P, a: Parameters<EquipmentUpdateArgs>) -> R {
        call!(self, p, equipment_update, a)
    }

    #[tool(
        description = "All equipment kinds defined so far.",
        annotations(title = "List equipment", read_only_hint = true, open_world_hint = false)
    )]
    async fn equipment_list(&self, p: P) -> R {
        call!(self, p, equipment_list)
    }

    #[tool(
        description = "Create an exercise with its muscle groups and ALL the equipment it requires. Search first to avoid duplicates.",
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
        description = "Find exercises by name (typos ok) and/or muscle group; with a gym, shows whether each is available there.",
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
        description = "Correct a workout's start/finish time, gym or notes.",
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
        description = "Correct a logged set (weight, reps, time, exercise, notes).",
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
        description = "Delete a logged set.",
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

    // ----- search & analysis -----
    #[tool(
        description = "Everything needed to recommend what to do next at a gym: doable exercises with last done/top set, days since each muscle group, and today's work.",
        annotations(title = "Training overview", read_only_hint = true, open_world_hint = false)
    )]
    async fn training_overview(&self, p: P, a: Parameters<OverviewArgs>) -> R {
        call!(self, p, training_overview, a)
    }

    #[tool(
        description = "Free-text search across gym/exercise/equipment names and all notes.",
        annotations(title = "Search", read_only_hint = true, open_world_hint = false)
    )]
    async fn search(&self, p: P, a: Parameters<SearchArgs>) -> R {
        call!(self, p, search, a)
    }

    #[tool(
        description = "Find sets by exercise, muscle group, equipment, gym, date range, weight and reps; sort by date, weight, reps, volume or estimated 1RM.",
        annotations(title = "Find sets", read_only_hint = true, open_world_hint = false)
    )]
    async fn set_search(&self, p: P, a: Parameters<SetSearchArgs>) -> R {
        call!(self, p, set_search, a)
    }

    #[tool(
        description = "Time series per day/week/month of top weight, estimated 1RM, volume, sets or reps, for an exercise or a muscle group.",
        annotations(title = "Progress over time", read_only_hint = true, open_world_hint = false)
    )]
    async fn progress(&self, p: P, a: Parameters<ProgressArgs>) -> R {
        call!(self, p, progress, a)
    }

    #[tool(
        description = "Personal records per exercise: heaviest set, best estimated 1RM, max reps at each weight, best volume workout.",
        annotations(title = "Personal records", read_only_hint = true, open_world_hint = false)
    )]
    async fn personal_records(&self, p: P, a: Parameters<RecordsArgs>) -> R {
        call!(self, p, personal_records, a)
    }

    #[tool(
        description = "Exercises that train the same muscles and can be done at the gym (default: the current one).",
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
        assert_eq!(tools.len(), 28);
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
        assert_eq!(hint("set_add").idempotent_hint, Some(false));
        assert_eq!(hint("workout_start").idempotent_hint, Some(true));
    }
}
