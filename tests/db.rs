//! Integration tests against a real Postgres. Needs DATABASE_URL (see README); each test gets a fresh database.

use std::time::Duration;

use chrono::{DateTime, DurationRound, Utc};
use gym_mcp::db::{App, AppError};
use gym_mcp::{body_note, measurement, search, setup, workout};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sqlx::PgPool;

fn app(pool: PgPool) -> App {
    App {
        pool,
        default_timezone: "Europe/Amsterdam".into(),
        stale_after: Duration::from_secs(3 * 3600),
    }
}

fn a<T: DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).expect("test args")
}

fn hours_ago(h: i64) -> String {
    (Utc::now() - chrono::Duration::hours(h)).to_rfc3339()
}

/// Finds a muscle group's row in a `muscle_groups` array.
trait GroupRow {
    fn group(&self, g: &str) -> &Value;
}

impl GroupRow for Value {
    fn group(&self, g: &str) -> &Value {
        let rows = self.as_array().unwrap_or_else(|| panic!("not rows: {self}"));
        rows.iter()
            .find(|r| r["muscle_group"] == g)
            .unwrap_or_else(|| panic!("no row for {g}"))
    }
}

fn user_msg(r: Result<Value, AppError>) -> String {
    match r {
        Err(AppError::User(m)) => m,
        other => panic!("expected a user error, got {other:?}"),
    }
}

const ME: &str = "alice";

async fn seed(app: &App) {
    setup::gym_create(
        app,
        ME,
        a(json!({"name": "Trainmore NDSM", "aliases": ["ndsm"],
                 "equipment": "leg press (max 300 kg), dumbbells 2-14 kg, flat benches. No cable tower."})),
    )
    .await
    .unwrap();
    setup::gym_create(app, ME, a(json!({"name": "Basic-Fit Zuid"})))
        .await
        .unwrap();
    for (name, groups, eq) in [
        ("Leg press", json!(["quads", "glutes"]), json!("leg press")),
        (
            "Dumbbell bench press",
            json!(["chest", "triceps"]),
            json!("dumbbells + flat bench"),
        ),
        ("Cable fly", json!(["chest"]), json!("cable tower")),
        ("Push-up", json!(["chest", "triceps"]), json!(null)),
    ] {
        setup::exercise_create(
            app,
            ME,
            a(json!({"name": name, "muscle_groups": groups, "equipment": eq})),
        )
        .await
        .unwrap();
    }
}

#[sqlx::test]
async fn start_resumes_same_gym_and_guards_other_gym(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let first = workout::start(&app, ME, a(json!({"gym": "ndsm trainmore"})))
        .await
        .unwrap();
    assert_eq!(first["status"], "started");
    let again = workout::start(&app, ME, a(json!({"gym": "NDSM"}))).await.unwrap();
    assert_eq!(again["status"], "resumed");
    assert_eq!(again["workout"]["workout_id"], first["workout"]["workout_id"]);

    let msg = user_msg(workout::start(&app, ME, a(json!({"gym": "basic-fit zuid"}))).await);
    assert!(msg.contains("still open at Trainmore NDSM"), "{msg}");

    let moved = workout::start(&app, ME, a(json!({"gym": "basic-fit zuid", "finish_open": true})))
        .await
        .unwrap();
    assert_eq!(moved["status"], "started");
    assert_eq!(moved["superseded"]["workout_id"], first["workout"]["workout_id"]);
}

#[sqlx::test]
async fn finish_closes_the_open_workout_but_not_before_its_last_set(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    workout::set_add(&app, ME, a(json!({"exercise": "push-up", "sets": [{"reps": 20}]})))
        .await
        .unwrap();

    let msg = user_msg(workout::finish(&app, ME, a(json!({"finished_at": hours_ago(1)}))).await);
    assert!(msg.contains("before the last logged set"), "{msg}");

    let fin = workout::finish(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(fin["status"], "finished");
    assert_eq!(fin["workout"]["finish_reason"], "manual");
    assert!(!fin["workout"]["finished_at"].is_null());

    let msg = user_msg(workout::finish(&app, ME, a(json!({}))).await);
    assert!(msg.contains("no open workout"), "{msg}");
}

#[sqlx::test]
async fn stale_workout_with_sets_closes_at_last_set(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(2)})))
        .await
        .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "sets": [{"reps": 10, "weight_kg": 100, "performed_at": hours_ago(1)}]})),
    )
    .await
    .unwrap();
    // Same data, seen by a server whose stale window is shorter than the gap since the last set.
    let impatient = App {
        stale_after: Duration::from_secs(1800),
        ..app.clone()
    };
    // Reads are read-only: they report the closure but don't save it…
    let cur = workout::current(&impatient, ME).await.unwrap();
    assert!(cur["open_workout"].is_null());
    assert_eq!(
        cur["closed_stale"]["action"],
        "stale: will be auto-finished on the next workout action"
    );
    let still_open = search::workout_list(&app, ME, a(json!({}))).await.unwrap();
    assert!(still_open[0]["finished_at"].is_null());
    // …the next workout action does, with the same outcome.
    let fin = workout::finish(&impatient, ME, a(json!({}))).await.unwrap();
    assert_eq!(fin["closed_stale"]["action"], "auto-finished (stale)");
    let past = search::workout_list(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(past[0]["finish_reason"], "stale");
    assert_eq!(
        past[0]["finished_at"],
        past[0]["exercises"][0]["sets"][0]["performed_at"]
    );
}

#[sqlx::test]
async fn stale_empty_workout_is_deleted_and_set_add_refuses(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(4)})))
        .await
        .unwrap();
    let msg = user_msg(workout::set_add(&app, ME, a(json!({"exercise": "push-up", "sets": [{"reps": 20}]}))).await);
    assert!(msg.contains("no open workout"), "{msg}");
    assert!(msg.contains("deleted"), "{msg}");
    assert_eq!(search::workout_list(&app, ME, a(json!({}))).await.unwrap(), json!([]));
}

#[sqlx::test]
async fn stale_workout_with_only_a_body_note_is_kept(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let w = workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(4)})))
        .await
        .unwrap();
    let wid = w["workout"]["workout_id"].clone();
    body_note::body_note_add(
        &app,
        ME,
        a(json!({"location": "lower_back", "severity": 7, "workout_id": wid, "noted_at": hours_ago(4)})),
    )
    .await
    .unwrap();
    // The ache is the only record of this visit: finish the workout instead of deleting it and orphaning the note.
    let next = workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    assert_eq!(next["closed_stale"]["action"], "auto-finished (stale)");
    let notes = body_note::body_note_list(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(notes[0]["workout_id"], wid);
}

#[sqlx::test]
async fn late_sets_order_and_move_start_back(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "benchpres", "sets": [{"reps": 10, "weight_kg": 12}, {"reps": 8, "weight_kg": 14}]})),
    )
    .await
    .unwrap();
    // A forgotten warm-up set from before "arrival".
    let r = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "dumbbell bench", "sets": [{"reps": 15, "weight_kg": 8, "performed_at": hours_ago(2)}]})),
    )
    .await
    .unwrap();
    assert_eq!(r["started_at_moved_back"], true);
    let sets = &r["workout"]["exercises"][0]["sets"];
    assert_eq!(sets[0]["weight_kg"], 8.0);
    assert_eq!(sets[0]["set_number"], 1);
    assert_eq!(sets[2]["weight_kg"], 14.0);
    assert_eq!(sets[2]["set_number"], 3);

    let future = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let msg = user_msg(
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": "push-up", "sets": [{"reps": 1, "performed_at": future}]})),
        )
        .await,
    );
    assert!(msg.contains("future"), "{msg}");
}

#[sqlx::test]
async fn past_workout_and_local_calendar(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    // Mon 2025-11-03 00:30 in Amsterdam = Sun 23:30 UTC.
    let w = workout::add_past(
        &app,
        ME,
        a(json!({"gym": "ndsm", "started_at": "2025-11-03 00:00", "finished_at": "2025-11-03 01:00"})),
    )
    .await
    .unwrap();
    let id = w["workout_id"].as_i64().unwrap();
    let msg = user_msg(
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": "leg press", "workout_id": id, "sets": [{"reps": 5}]})),
        )
        .await,
    );
    assert!(msg.contains("performed_at"), "{msg}");
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "workout_id": id, "sets": [{"reps": 5, "weight_kg": 120, "performed_at": "2025-11-03 00:30"}]})),
    )
    .await
    .unwrap();

    let week = search::progress(
        &app,
        ME,
        a(json!({"exercise": "leg press", "metric": "top_weight", "bucket": "week"})),
    )
    .await
    .unwrap();
    assert_eq!(week["series"][0]["period_start"], "2025-11-03");
    setup::settings_update(&app, ME, a(json!({"timezone": "UTC"})))
        .await
        .unwrap();
    let week = search::progress(
        &app,
        ME,
        a(json!({"exercise": "leg press", "metric": "top_weight", "bucket": "week"})),
    )
    .await
    .unwrap();
    assert_eq!(
        week["series"][0]["period_start"], "2025-10-27",
        "in UTC it was still Sunday"
    );

    let msg = user_msg(setup::settings_update(&app, ME, a(json!({"timezone": "Mars/Olympus"}))).await);
    assert!(msg.contains("invalid"), "{msg}");
}

#[sqlx::test]
async fn search_records_and_overview(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "sets": [
            {"reps": 12, "weight_kg": 100}, {"reps": 8, "weight_kg": 140, "notes": "seat on position 5"}, {"reps": 10, "weight_kg": 120}
        ]})),
    )
    .await
    .unwrap();

    let heavy = search::set_search(
        &app,
        ME,
        a(json!({"exercise": "leg press", "order_by": "weight", "limit": 1})),
    )
    .await
    .unwrap();
    assert_eq!(heavy[0]["weight_kg"], 140.0);
    let light = search::set_search(&app, ME, a(json!({"muscle_group": "quads", "max_weight_kg": 110})))
        .await
        .unwrap();
    assert_eq!(light.as_array().unwrap().len(), 1);

    let pr = search::personal_records(&app, ME, a(json!({"exercise": "leg press"})))
        .await
        .unwrap();
    assert_eq!(pr[0]["heaviest_set"]["weight_kg"], 140.0);
    assert_eq!(pr[0]["best_e1rm"]["weight_kg"], 140.0); // 140*(1+8/30)=177.3 > 120*(1+10/30)=160
    assert_eq!(pr[0]["best_volume_workout"]["volume_kg"], 3520.0);
    let reps = pr[0]["max_reps_by_weight_kg"].as_array().unwrap();
    let weights: Vec<f64> = reps.iter().map(|r| r["weight_kg"].as_f64().unwrap()).collect();
    assert!(weights.is_sorted(), "sorted by weight: {weights:?}");
    let at140 = reps.iter().find(|r| r["weight_kg"] == 140.0).unwrap();
    assert_eq!(at140["reps"], 8);

    let hits = search::search(&app, ME, a(json!({"query": "seat position"})))
        .await
        .unwrap();
    assert_eq!(hits["notes"][0]["type"], "set");
    let hits = search::search(&app, ME, a(json!({"query": "legpress"}))).await.unwrap();
    assert!(
        hits["names"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["name"] == "Leg press")
    );

    let o = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(o["overview"]["muscle_groups"].group("quads")["days_since"], 0);
    assert!(o["overview"]["muscle_groups"].group("chest")["days_since"].is_null());
    let rows = o["overview"]["muscle_groups"].as_array().unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["muscle_group"].as_str().unwrap()).collect();
    assert_eq!(names, setup::MUSCLE_GROUPS, "every group, in a stable order");
    assert_eq!(o["overview"]["done_today"][0]["sets"], 3);

    let alt = search::exercise_alternatives(&app, ME, a(json!({"exercise": "push-up"})))
        .await
        .unwrap();
    let names: Vec<_> = alt["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["Dumbbell bench press", "Cable fly"],
        "most shared muscle groups first"
    );
    assert!(
        alt["gym"]["equipment"].as_str().unwrap().contains("No cable tower"),
        "Claude gets the gym text to judge"
    );
}

#[sqlx::test]
async fn search_ranks_notes_before_limiting(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "sets": [
            {"reps": 10, "notes": "knee fine"}, {"reps": 10, "notes": "knee fine"}, {"reps": 10, "notes": "knee fine"},
            {"reps": 10, "notes": "knee knee sharp knee pain"}
        ]})),
    )
    .await
    .unwrap();

    let hits = search::search(&app, ME, a(json!({"query": "knee", "limit": 2})))
        .await
        .unwrap();
    let notes = hits["notes"].as_array().unwrap();
    assert_eq!(notes.len(), 2);
    assert_eq!(
        notes[0]["text"], "knee knee sharp knee pain",
        "the best match survives the limit"
    );
}

#[sqlx::test]
async fn exercise_search_lists_every_match(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    for name in ["Dumbbell row", "Dumbbell shoulder press"] {
        setup::exercise_create(&app, ME, a(json!({"name": name})))
            .await
            .unwrap();
    }
    let ex = setup::exercise_search(&app, ME, a(json!({"query": "dumbbell"})))
        .await
        .unwrap();
    let mut names: Vec<&str> = ex
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names[0], "Dumbbell row", "closest first (fewest extra words)");
    names.sort_unstable();
    assert_eq!(
        names,
        ["Dumbbell bench press", "Dumbbell row", "Dumbbell shoulder press"],
        "not just the closest one"
    );
    let id = ex[0]["id"].to_string();
    let by_id = setup::exercise_search(&app, ME, a(json!({"query": id}))).await.unwrap();
    assert_eq!(by_id[0]["name"], "Dumbbell row");
    let long = setup::exercise_search(&app, ME, a(json!({"query": "seated dumbbell row narrow grip"})))
        .await
        .unwrap();
    assert_eq!(long[0]["name"], "Dumbbell row", "no good match still lists the closest");
}

#[sqlx::test]
async fn owners_are_isolated(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let msg = user_msg(setup::gym_get(&app, "bob", a(json!({"gym": "ndsm"}))).await);
    assert!(msg.contains("no gym matches"), "{msg}");
    let gym_id = setup::gym_get(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap()["id"].to_string();
    assert!(user_msg(setup::gym_get(&app, "bob", a(json!({"gym": gym_id}))).await).contains("no gym"));
    let w = workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let wid = w["workout"]["workout_id"].as_i64().unwrap();
    assert!(
        user_msg(
            workout::set_add(
                &app,
                "bob",
                a(json!({"exercise": "x", "workout_id": wid, "sets": [{"reps": 1}]}))
            )
            .await
        )
        .contains("no workout")
    );
}

#[sqlx::test]
async fn effort_rest_and_goal_feed_recommendations(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let s = setup::settings_update(&app, ME, a(json!({"goal": "hypertrophy"})))
        .await
        .unwrap();
    assert_eq!(s["goal"], "hypertrophy");
    assert_eq!(s["timezone"], "Europe/Amsterdam", "goal-only update keeps the timezone");
    assert!(user_msg(setup::settings_update(&app, ME, a(json!({}))).await).contains("at least one setting"));

    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let r = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "dumbbell bench press", "sets": [
            {"reps": 10, "weight_kg": 12, "rpe": 7, "rest_s": 90},
            {"reps": 8, "weight_kg": 13, "rpe": 8.5},
            {"reps": 8, "weight_kg": 13, "rpe": 9}
        ]})),
    )
    .await
    .unwrap();
    assert_eq!(r["added"], 3);
    let first = &r["workout"]["exercises"][0]["sets"][0];
    assert_eq!((first["rpe"].as_f64(), first["rest_s"].as_i64()), (Some(7.0), Some(90)));

    for bad in [json!(11), json!(7.3), json!(0)] {
        let msg = user_msg(
            workout::set_add(
                &app,
                ME,
                a(json!({"exercise": "push-up", "sets": [{"reps": 5, "rpe": bad}]})),
            )
            .await,
        );
        assert!(msg.contains("invalid value"), "rpe {bad}: {msg}");
    }

    let set_id = first["set_id"].as_i64().unwrap();
    let w = workout::set_update(&app, ME, a(json!({"set_id": set_id, "rpe": 6.5})))
        .await
        .unwrap();
    assert_eq!(w["exercises"][0]["sets"][0]["rpe"], 6.5);
    assert_eq!(w["exercises"][0]["sets"][0]["rest_s"], 90, "unset fields stay");

    let o = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(o["overview"]["goal"], "hypertrophy");
    let bench = o["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Dumbbell bench press")
        .unwrap();
    let last: Vec<_> = bench["last_session"]["sets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["rpe"].as_f64().unwrap())
        .collect();
    assert_eq!(last, [6.5, 8.5, 9.0]);

    let hard = search::set_search(&app, ME, a(json!({"exercise": "dumbbell bench press", "limit": 1})))
        .await
        .unwrap();
    assert_eq!(hard[0]["rpe"], 9.0);
}

#[sqlx::test]
async fn gym_append_adds_a_line(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let up = |v: Value| setup::gym_update(&app, ME, a(v));
    let g = up(json!({"gym": "Basic-Fit Zuid", "equipment_append": " cable tower ", "notes_append": "busy at 18:00"}))
        .await
        .unwrap();
    assert_eq!(g["equipment"], "cable tower", "empty text: just the line, trimmed");
    assert_eq!(g["notes"], "busy at 18:00");
    let g = up(json!({"gym": "Basic-Fit Zuid", "equipment_append": "sled", "notes_append": "sauna"}))
        .await
        .unwrap();
    assert_eq!(g["equipment"], "cable tower\nsled");
    assert_eq!(g["notes"], "busy at 18:00\nsauna");

    let err = up(json!({"gym": "Basic-Fit Zuid", "equipment": "x", "equipment_append": "y"})).await;
    assert!(err.unwrap_err().to_string().contains("not both"));
    let err = up(json!({"gym": "Basic-Fit Zuid", "notes": "x", "notes_append": "y"})).await;
    assert!(err.is_err());
    let err = up(json!({"gym": "Basic-Fit Zuid", "equipment_append": "  "})).await;
    assert!(err.unwrap_err().to_string().contains("empty"));
    let err = up(json!({"gym": "Basic-Fit Zuid", "notes_append": ""})).await;
    assert!(err.unwrap_err().to_string().contains("empty"));
    let g = setup::gym_get(&app, ME, a(json!({"gym": "Basic-Fit Zuid"})))
        .await
        .unwrap();
    assert_eq!(g["equipment"], "cable tower\nsled", "unchanged after the error");
}

#[sqlx::test]
async fn equipment_is_free_text_on_gyms_and_exercises(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let g = setup::gym_get(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    assert!(g["equipment"].as_str().unwrap().starts_with("leg press (max 300 kg)"));
    let g = setup::gym_update(
        &app,
        ME,
        a(json!({"gym": "ndsm", "equipment": "leg press, dumbbells 2-40 kg, a sled"})),
    )
    .await
    .unwrap();
    assert_eq!(g["equipment"], "leg press, dumbbells 2-40 kg, a sled");
    let g = setup::gym_update(&app, ME, a(json!({"gym": "ndsm", "notes": "sauna"})))
        .await
        .unwrap();
    assert_eq!(
        g["equipment"], "leg press, dumbbells 2-40 kg, a sled",
        "unset fields stay"
    );

    let ex = setup::exercise_search(&app, ME, a(json!({"query": "dumbbell bench"})))
        .await
        .unwrap();
    assert_eq!(ex[0]["equipment"], "dumbbells + flat bench");
    let ex = setup::exercise_search(&app, ME, a(json!({"query": "push-up"})))
        .await
        .unwrap();
    assert!(ex[0]["equipment"].is_null(), "bodyweight");

    let hits = search::search(&app, ME, a(json!({"query": "sled"}))).await.unwrap();
    assert_eq!(hits["notes"][0]["type"], "gym_equipment");
    let hits = search::search(&app, ME, a(json!({"query": "cable"}))).await.unwrap();
    assert!(
        hits["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["type"] == "exercise_equipment" && h["name"] == "Cable fly")
    );

    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let o = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(
        o["overview"]["gym"]["equipment"],
        "leg press, dumbbells 2-40 kg, a sled"
    );
    assert_eq!(
        o["overview"]["exercises"].as_array().unwrap().len(),
        4,
        "all exercises; Claude judges the fit"
    );

    let sets = search::set_search(&app, ME, a(json!({"equipment": "dumbbell"})))
        .await
        .unwrap();
    assert_eq!(sets, json!([]));
}

/// Migration 0004 folds the old structured equipment into the new text fields.
#[sqlx::test(migrations = false)]
async fn migration_keeps_structured_equipment_as_text(pool: PgPool) {
    for m in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_equipment_description.sql"),
        include_str!("../migrations/0003_effort_rest_goal.sql"),
    ] {
        sqlx::raw_sql(m).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO gym (owner_sub, name, inventory_complete) VALUES ('a', 'NDSM', true);
         INSERT INTO equipment (owner_sub, name, category, description) VALUES
            ('a', 'leg press', 'machine', '45° sled'), ('a', 'dumbbells', 'free_weight', ''), ('a', 'cable tower', 'machine', '');
         INSERT INTO gym_equipment (gym_id, equipment_id, status, quantity, available_weights_kg, max_load_kg, notes) VALUES
            (1, 1, 'present', 2, NULL, 300, 'upstairs'), (1, 2, 'present', NULL, '{2,4,6}', NULL, ''), (1, 3, 'absent', NULL, NULL, NULL, '');
         INSERT INTO exercise (owner_sub, name) VALUES ('a', 'DB bench');
         INSERT INTO exercise_equipment VALUES (1, 2), (1, 1);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0004_equipment_free_text.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let gym: String = sqlx::query_scalar("SELECT equipment FROM gym")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        gym,
        "dumbbells (free weight); 2, 4, 6 kg\nleg press (machine): 45° sled; 2×; max 300 kg; upstairs\nNot available: cable tower\nThis list is complete."
    );
    let ex: String = sqlx::query_scalar("SELECT equipment FROM exercise")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(ex, "dumbbells + leg press");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE tablename LIKE '%equipment%'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test]
async fn cardio_distance_pace_and_records(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let ex = setup::exercise_create(&app, ME, a(json!({"name": "Treadmill run", "kind": "cardio"})))
        .await
        .unwrap();
    assert_eq!(ex["kind"], "cardio");
    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let added = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "treadmill", "sets": [
            {"duration_s": 1500, "distance_m": 5000, "intensity": "incline 1%"},
            {"duration_s": 1400, "distance_m": 5000}]})),
    )
    .await
    .unwrap();
    let sets = &added["workout"]["exercises"][0]["sets"];
    assert_eq!(sets[0]["intensity"], "incline 1%");
    assert_eq!(sets[1]["distance_m"], 5000.0);

    let pace = search::progress(&app, ME, a(json!({"exercise": "treadmill", "metric": "pace"})))
        .await
        .unwrap();
    assert_eq!(pace["series"][0]["value"], 280.0);
    let dist = search::progress(&app, ME, a(json!({"exercise": "treadmill", "metric": "distance"})))
        .await
        .unwrap();
    assert_eq!(dist["series"][0]["value"], 10000.0);

    let pr = search::personal_records(&app, ME, a(json!({"exercise": "treadmill"})))
        .await
        .unwrap();
    assert_eq!(pr[0]["best_pace"]["pace_s_per_km"], 280.0);
    assert_eq!(pr[0]["longest_duration"]["duration_s"], 1500);

    let fastest = search::set_search(&app, ME, a(json!({"order_by": "pace", "desc": false})))
        .await
        .unwrap();
    assert_eq!(fastest[0]["duration_s"], 1400);

    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(
        ov["overview"]["cardio_this_week"],
        json!({"minutes": 48, "distance_km": 10.0})
    );

    let msg = user_msg(
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": "treadmill", "sets": [{"intensity": "level 3"}]})),
        )
        .await,
    );
    assert!(msg.contains("distance_m"), "{msg}");
}

#[sqlx::test(migrations = false)]
async fn migration_turns_cardio_group_into_kind(pool: PgPool) {
    for m in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_equipment_description.sql"),
        include_str!("../migrations/0003_effort_rest_goal.sql"),
        include_str!("../migrations/0004_equipment_free_text.sql"),
    ] {
        sqlx::raw_sql(m).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO exercise (owner_sub, name, muscle_groups) VALUES
            ('a', 'Bike', '{cardio,quads}'), ('a', 'Squat', '{quads}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0005_cardio.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let kinds: Vec<(String, String)> = sqlx::query_as("SELECT name, kind FROM exercise ORDER BY name")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        kinds,
        [("Bike".into(), "cardio".into()), ("Squat".into(), "strength".into())]
    );
}

#[sqlx::test]
async fn muscle_groups_primary_secondary_and_aliases(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::exercise_create(
        &app,
        ME,
        a(json!({"name": "Barbell bench press", "muscle_groups": ["chest"],
                 "secondary_muscle_groups": ["triceps", "front_delts"]})),
    )
    .await
    .unwrap();
    setup::exercise_create(
        &app,
        ME,
        a(json!({"name": "Lat pulldown", "muscle_groups": ["lats"], "secondary_muscle_groups": ["biceps"]})),
    )
    .await
    .unwrap();
    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    for ex in ["barbell bench", "lat pulldown"] {
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": ex, "sets": [{"reps": 8, "weight_kg": 50}]})),
        )
        .await
        .unwrap();
    }

    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    let groups = &ov["overview"]["muscle_groups"];
    assert_eq!(groups.group("chest")["days_since"], 0);
    assert!(
        groups.group("triceps")["days_since"].is_null(),
        "secondary doesn't count: {groups}"
    );
    assert!(
        groups
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["muscle_group"] != "back" && r["muscle_group"] != "cardio"),
        "{groups}"
    );

    let back = search::set_search(&app, ME, a(json!({"muscle_group": "back"})))
        .await
        .unwrap();
    assert_eq!(back.as_array().unwrap().len(), 1);
    assert_eq!(back[0]["exercise"], "Lat pulldown");
    let found = setup::exercise_search(&app, ME, a(json!({"muscle_group": "shoulders"})))
        .await
        .unwrap();
    assert_eq!(
        found.as_array().unwrap().len(),
        0,
        "front_delts is only secondary on bench"
    );

    let alt = search::exercise_alternatives(&app, ME, a(json!({"exercise": "barbell bench"})))
        .await
        .unwrap();
    assert_eq!(alt["alternatives"][0]["name"], "Dumbbell bench press");

    let coarse = user_msg(setup::exercise_create(&app, ME, a(json!({"name": "Row", "muscle_groups": ["back"]}))).await);
    assert!(coarse.contains("too coarse") && coarse.contains("lats"), "{coarse}");
    let overlap = user_msg(
        setup::exercise_create(
            &app,
            ME,
            a(json!({"name": "Dip", "muscle_groups": ["triceps"], "secondary_muscle_groups": ["triceps"]})),
        )
        .await,
    );
    assert!(overlap.contains("both primary and secondary"), "{overlap}");
    let stored_overlap = setup::exercise_update(
        &app,
        ME,
        a(json!({"exercise": "lat pulldown", "secondary_muscle_groups": ["lats"]})),
    )
    .await;
    assert!(matches!(stored_overlap, Err(AppError::User(_))), "{stored_overlap:?}");
    let bad_filter = user_msg(search::set_search(&app, ME, a(json!({"muscle_group": "pecs"}))).await);
    assert!(bad_filter.contains("unknown muscle group"), "{bad_filter}");
}

#[sqlx::test]
async fn exercise_update_changes_only_the_fields_passed(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::exercise_update(
        &app,
        ME,
        a(json!({"exercise": "leg press", "description": "45° sled", "aliases": ["lp"], "secondary_muscle_groups": ["calves"]})),
    )
    .await
    .unwrap();

    let e = setup::exercise_update(&app, ME, a(json!({"exercise": "lp", "equipment": "leg press sled"})))
        .await
        .unwrap();
    assert_eq!(e["equipment"], "leg press sled");
    assert_eq!(e["description"], "45° sled");
    assert_eq!(e["aliases"], json!(["lp"]));
    assert_eq!(e["muscle_groups"], json!(["quads", "glutes"]));
    assert_eq!(e["kind"], "strength");
    assert_eq!(e["secondary_muscle_groups"], json!(["calves"]));

    let e = setup::exercise_update(
        &app,
        ME,
        a(json!({"exercise": "lp", "muscle_groups": ["quads", "glutes", "hamstrings"]})),
    )
    .await
    .unwrap();
    assert_eq!(e["muscle_groups"], json!(["quads", "glutes", "hamstrings"]));
    assert_eq!(e["secondary_muscle_groups"], json!(["calves"]));

    let e = setup::exercise_update(&app, ME, a(json!({"exercise": "lp", "name": "  Leg press 45  "})))
        .await
        .unwrap();
    assert_eq!(e["name"], "Leg press 45");
    assert_eq!(e["equipment"], "leg press sled");

    let msg = user_msg(setup::exercise_update(&app, "bob", a(json!({"exercise": "lp", "name": "mine"}))).await);
    assert!(msg.contains("no exercise matches"), "{msg}");
}

#[sqlx::test]
async fn gym_list_is_sorted_by_name_with_last_workout(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let gyms = setup::gym_list(&app, ME).await.unwrap();
    assert_eq!(gyms[0]["name"], "Basic-Fit Zuid");
    assert_eq!(gyms[1]["name"], "Trainmore NDSM");
    assert_eq!(gyms[1]["aliases"], json!(["ndsm"]));
    assert!(gyms[1]["last_workout_at"].is_null());

    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let gyms = setup::gym_list(&app, ME).await.unwrap();
    assert!(!gyms[1]["last_workout_at"].is_null());
    assert!(gyms[0]["last_workout_at"].is_null());
    assert_eq!(setup::gym_list(&app, "bob").await.unwrap(), json!([]));
}

#[sqlx::test(migrations = false)]
async fn migration_maps_coarse_muscle_groups(pool: PgPool) {
    for m in [
        include_str!("../migrations/0001_init.sql"),
        include_str!("../migrations/0002_equipment_description.sql"),
        include_str!("../migrations/0003_effort_rest_goal.sql"),
        include_str!("../migrations/0004_equipment_free_text.sql"),
        include_str!("../migrations/0005_cardio.sql"),
    ] {
        sqlx::raw_sql(m).execute(&pool).await.unwrap();
    }
    sqlx::raw_sql(
        "INSERT INTO exercise (owner_sub, name, muscle_groups) VALUES
            ('a', 'Row', '{back,biceps}'), ('a', 'Press', '{shoulders,triceps}'), ('a', 'Bike', '{cardio,quads}')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/0006_muscle_groups.sql"))
        .execute(&pool)
        .await
        .unwrap();
    let rows: Vec<(String, Vec<String>)> = sqlx::query_as("SELECT name, muscle_groups FROM exercise ORDER BY name")
        .fetch_all(&pool)
        .await
        .unwrap();
    let rows: Vec<(&str, Vec<&str>)> = rows
        .iter()
        .map(|(n, g)| (n.as_str(), g.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        rows,
        [
            ("Bike", vec!["quads"]),
            ("Press", vec!["front_delts", "side_delts", "triceps"]),
            ("Row", vec!["lats", "upper_back", "biceps"]),
        ]
    );
}

/// A finished workout `days` ago at `gym` with one leg press set; returns set_add's result.
async fn past_leg_press(app: &App, gym: &str, days: i64, kg: f64, machine: Option<&str>) -> Value {
    let start = Utc::now() - chrono::Duration::days(days);
    let w = workout::add_past(
        app,
        ME,
        a(json!({"gym": gym, "started_at": start.to_rfc3339(),
                 "finished_at": (start + chrono::Duration::hours(1)).to_rfc3339()})),
    )
    .await
    .unwrap();
    let mut args = json!({"exercise": "leg press", "workout_id": w["workout_id"],
                          "sets": [{"reps": 10, "weight_kg": kg,
                                    "performed_at": (start + chrono::Duration::minutes(10)).to_rfc3339()}]});
    if let Some(m) = machine {
        args["machine"] = json!(m);
    }
    workout::set_add(app, ME, a(args)).await.unwrap()
}

#[sqlx::test]
async fn machine_defaults_per_gym_and_groups_history(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::gym_create(&app, ME, a(json!({"name": "Trainmore Centrum"})))
        .await
        .unwrap();

    past_leg_press(&app, "centrum", 5, 90.0, Some("Technogym")).await;
    let given = past_leg_press(&app, "ndsm", 4, 100.0, Some(" Technogym ")).await;
    assert_eq!(
        (given["machine"].clone(), given["machine_source"].clone()),
        (json!("Technogym"), json!("given"))
    );
    let inherited = past_leg_press(&app, "ndsm", 3, 110.0, None).await;
    assert_eq!(inherited["machine"], "Technogym");
    assert_eq!(inherited["machine_source"], "last_used_here");
    let elsewhere = past_leg_press(&app, "basic-fit", 2, 80.0, None).await;
    assert!(elsewhere["machine"].is_null());
    assert_eq!(elsewhere["machine_source"], "none");
    past_leg_press(&app, "ndsm", 1, 60.0, Some("matrix")).await;

    // At Centrum the machine is Technogym, so last_session is NDSM's Technogym session, not the newer Matrix or Basic-Fit one.
    let ov = search::training_overview(&app, ME, a(json!({"gym": "centrum"})))
        .await
        .unwrap();
    let lp = ov["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Leg press")
        .unwrap();
    assert_eq!(lp["machine_here"], "Technogym");
    assert_eq!(lp["last_session"]["gym"], "Trainmore NDSM");
    assert_eq!(lp["last_session"]["sets"][0]["weight_kg"], 110.0);
    assert_eq!(lp["machines_used"], json!(["matrix", "Technogym"]));
    // At NDSM the latest machine is Matrix.
    let ov = search::training_overview(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    let lp = ov["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Leg press")
        .unwrap();
    assert_eq!(lp["last_session"]["sets"][0]["weight_kg"], 60.0);

    let rec = search::personal_records(&app, ME, a(json!({"exercise": "leg press"})))
        .await
        .unwrap();
    let by_machine: Vec<(Value, Value)> = rec
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["machine"].clone(), r["heaviest_set"]["weight_kg"].clone()))
        .collect();
    assert_eq!(
        by_machine,
        [
            (json!(null), json!(80)),
            (json!("matrix"), json!(60)),
            (json!("Technogym"), json!(110))
        ]
    );

    let series = search::progress(
        &app,
        ME,
        a(json!({"exercise": "leg press", "metric": "top_weight", "bucket": "day"})),
    )
    .await
    .unwrap();
    let mut machines: Vec<Value> = series["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["machine"].clone())
        .collect();
    machines.dedup();
    assert_eq!(machines, [json!(null), json!("matrix"), json!("Technogym")]);
    let techno = search::progress(
        &app,
        ME,
        a(json!({"exercise": "leg press", "metric": "top_weight", "bucket": "day", "machine": "techno"})),
    )
    .await
    .unwrap();
    assert_eq!(techno["series"].as_array().unwrap().len(), 3);

    let found = search::set_search(&app, ME, a(json!({"machine": "TECHNO"})))
        .await
        .unwrap();
    assert_eq!(found.as_array().unwrap().len(), 3);
    let set_id = found[0]["set_id"].as_i64().unwrap();
    let w = workout::set_update(&app, ME, a(json!({"set_id": set_id, "machine": ""})))
        .await
        .unwrap();
    assert!(w["exercises"][0]["sets"][0]["machine"].is_null());
}

#[sqlx::test]
async fn muscle_group_volume_in_overview(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::exercise_create(
        &app,
        ME,
        a(json!({"name": "Barbell bench press", "muscle_groups": ["chest"],
                 "secondary_muscle_groups": ["triceps", "front_delts"]})),
    )
    .await
    .unwrap();
    setup::exercise_create(
        &app,
        ME,
        a(json!({"name": "Bike", "kind": "cardio", "muscle_groups": ["quads"]})),
    )
    .await
    .unwrap();

    let start = Utc::now() - chrono::Duration::days(10);
    let past = workout::add_past(
        &app,
        ME,
        a(json!({"gym": "ndsm", "started_at": start.to_rfc3339(),
                 "finished_at": (start + chrono::Duration::hours(1)).to_rfc3339()})),
    )
    .await
    .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "barbell bench", "workout_id": past["workout_id"],
                 "sets": [{"reps": 8, "weight_kg": 60, "rpe": 7,
                           "performed_at": (start + chrono::Duration::minutes(5)).to_rfc3339()}]})),
    )
    .await
    .unwrap();

    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "barbell bench", "sets": [{"reps": 8, "weight_kg": 60, "rpe": 8}, {"reps": 8, "weight_kg": 60}]})),
    )
    .await
    .unwrap();
    workout::set_add(&app, ME, a(json!({"exercise": "bike", "sets": [{"duration_s": 600}]})))
        .await
        .unwrap();

    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    let g = &ov["overview"]["muscle_groups"];
    assert_eq!(g.group("chest")["days_since"], 0);
    assert_eq!(g.group("chest")["sets_7d"], 2.0);
    assert_eq!(g.group("chest")["sets_28d"], 3.0);
    assert_eq!(g.group("chest")["train_days_28d"], 2);
    assert_eq!(g.group("chest")["avg_rpe_28d"], 7.5, "the set without rpe is ignored");
    assert_eq!(g.group("triceps")["sets_7d"], 1.0, "secondary counts half");
    assert_eq!(g.group("triceps")["sets_28d"], 1.5);
    assert!(g.group("triceps")["days_since"].is_null(), "secondary only");
    assert_eq!(g.group("triceps")["train_days_28d"], 0);
    assert_eq!(g.group("quads")["sets_28d"], 0.0, "cardio doesn't count");
    assert!(g.group("calves")["days_since"].is_null());
    assert_eq!(g.group("calves")["sets_7d"], 0.0);
    assert!(g.group("calves")["avg_rpe_28d"].is_null());
    assert_eq!(g.as_array().unwrap().len(), setup::MUSCLE_GROUPS.len());
}

#[sqlx::test]
async fn muscle_group_volume_over_any_period(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::exercise_create(
        &app,
        ME,
        a(json!({"name": "Barbell bench press", "muscle_groups": ["chest"],
                 "secondary_muscle_groups": ["triceps", "front_delts"]})),
    )
    .await
    .unwrap();
    let start = Utc::now() - chrono::Duration::days(10);
    let past = workout::add_past(
        &app,
        ME,
        a(json!({"gym": "ndsm", "started_at": start.to_rfc3339(),
                 "finished_at": (start + chrono::Duration::hours(1)).to_rfc3339()})),
    )
    .await
    .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "barbell bench", "workout_id": past["workout_id"],
                 "sets": [{"reps": 8, "weight_kg": 60, "rpe": 7,
                           "performed_at": (start + chrono::Duration::minutes(5)).to_rfc3339()}]})),
    )
    .await
    .unwrap();
    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "barbell bench", "sets": [{"reps": 8, "weight_kg": 60, "rpe": 8}, {"reps": 8, "weight_kg": 60}]})),
    )
    .await
    .unwrap();

    let vol = |args: Value| search::muscle_group_volume(&app, ME, a(args));
    let d14 = vol(json!({"days": 14})).await.unwrap();
    let chest = &d14["muscle_groups"].group("chest");
    assert_eq!(d14["days"], 14.0);
    assert_eq!(chest["sets"], 3.0);
    assert_eq!(chest["sets_per_week"], 1.5);
    assert_eq!(
        chest["from_exercises"],
        json!([{"exercise": "Barbell bench press", "role": "primary", "sets": 3.0, "reps": 24.0}])
    );
    assert_eq!(d14["muscle_groups"].group("triceps")["sets"], 1.5);
    assert_eq!(d14["muscle_groups"].group("calves")["from_exercises"], json!([]));

    let ranged =
        vol(json!({"from": (Utc::now() - chrono::Duration::days(14)).to_rfc3339(), "to": Utc::now().to_rfc3339()}))
            .await
            .unwrap();
    assert_eq!(ranged["muscle_groups"].group("chest")["sets"], 3.0);

    // A period that ends before today's sets: only the old set, but days_since is all-time.
    let old = vol(json!({"from": (Utc::now() - chrono::Duration::days(14)).to_rfc3339(),
                         "to": (Utc::now() - chrono::Duration::days(5)).to_rfc3339()}))
    .await
    .unwrap();
    assert_eq!(old["muscle_groups"].group("chest")["sets"], 1.0);
    assert_eq!(old["muscle_groups"].group("chest")["avg_rpe"], 7.0);
    assert_eq!(old["muscle_groups"].group("chest")["days_since"], 0);

    // The overview uses the same numbers.
    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    let og = &ov["overview"]["muscle_groups"].group("chest");
    let d7 = vol(json!({})).await.unwrap();
    let d28 = vol(json!({"days": 28})).await.unwrap();
    assert_eq!(og["sets_7d"], d7["muscle_groups"].group("chest")["sets"]);
    assert_eq!(og["sets_28d"], d28["muscle_groups"].group("chest")["sets"]);
    assert_eq!(og["train_days_28d"], d28["muscle_groups"].group("chest")["train_days"]);
    assert_eq!(og["avg_rpe_28d"], d28["muscle_groups"].group("chest")["avg_rpe"]);
    assert_eq!(d7["muscle_groups"].group("chest")["sets"], 2.0);

    for (args, msg) in [
        (json!({"days": 7, "from": "2026-10-01"}), "not both"),
        (json!({"to": "2026-10-01"}), "needs from"),
        (json!({"days": 0}), "between 1 and 365"),
        (json!({"from": "2026-10-02", "to": "2026-10-01"}), "before"),
    ] {
        let m = user_msg(vol(args).await);
        assert!(m.contains(msg), "{m}");
    }
}

/// Reps and exercises per muscle, every exercise with its role, the muscle_groups filter and local-date ranges.
#[sqlx::test]
async fn muscle_group_volume_reps_filters_and_ranges(pool: PgPool) {
    let app = app(pool.clone());
    seed(&app).await;
    for (name, primary, secondary) in [
        ("Pec deck", json!(["chest"]), json!([])),
        ("Incline press", json!(["chest"]), json!([])),
        ("Dips", json!(["chest"]), json!(["triceps"])),
        ("One-arm row", json!(["lats"]), json!(["biceps"])),
    ] {
        setup::exercise_create(
            &app,
            ME,
            a(json!({"name": name, "muscle_groups": primary, "secondary_muscle_groups": secondary})),
        )
        .await
        .unwrap();
    }
    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    // Six chest exercises (more than the old top-5 cut), one working set of 10 each.
    for ex in [
        "Dumbbell bench press",
        "Cable fly",
        "Push-up",
        "Pec deck",
        "Incline press",
        "Dips",
    ] {
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": ex, "sets": [{"reps": 10, "weight_kg": 20}]})),
        )
        .await
        .unwrap();
    }
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "One-arm row", "unilateral": true, "sets": [{"reps": 10, "weight_kg": 20}]})),
    )
    .await
    .unwrap();

    let vol = |args: Value| search::muscle_group_volume(&app, ME, a(args));
    let v = vol(json!({"days": 3})).await.unwrap();
    let rows = &v["muscle_groups"];
    let chest = rows.group("chest");
    assert_eq!(
        (chest["sets"].clone(), chest["reps"].clone(), chest["exercises"].clone()),
        (json!(6.0), json!(60.0), json!(6))
    );
    assert_eq!(chest["from_exercises"].as_array().unwrap().len(), 6);
    // Primary counts 1, secondary 0.5, for reps as for sets.
    let triceps = rows.group("triceps");
    assert_eq!(
        (
            triceps["sets"].clone(),
            triceps["reps"].clone(),
            triceps["exercises"].clone()
        ),
        (json!(2.5), json!(25.0), json!(3))
    );
    let dips = triceps["from_exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["exercise"] == "Dips")
        .unwrap();
    assert_eq!(
        dips,
        &json!({"exercise": "Dips", "role": "secondary", "sets": 0.5, "reps": 5.0})
    );
    // Unilateral: once per side.
    assert_eq!(
        (rows.group("lats")["sets"].clone(), rows.group("lats")["reps"].clone()),
        (json!(2.0), json!(20.0))
    );
    assert_eq!(rows.group("biceps")["reps"], 10.0);
    assert_eq!(rows.group("calves")["reps"], 0.0);
    assert_eq!(rows.group("calves")["exercises"], 0);

    // Filter: coarse names expand, overlap collapses, rows keep the canonical order.
    let some = vol(json!({"days": 3, "muscle_groups": ["triceps", "back", "lats"]}))
        .await
        .unwrap();
    let names: Vec<&str> = some["muscle_groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["muscle_group"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["lats", "upper_back", "lower_back", "triceps"]);
    let msg = user_msg(vol(json!({"muscle_groups": ["pecs"]})).await);
    assert!(msg.starts_with("unknown muscle group 'pecs'"), "{msg}");
    let msg = user_msg(vol(json!({"muscle_groups": []})).await);
    assert!(msg.starts_with("muscle_groups must not be empty"), "{msg}");

    // Any range with local dates, in the user's timezone (Europe/Amsterdam here): today's sets fall in
    // [today, tomorrow), and the range starts at local midnight.
    let today: (String, String) = sqlx::query_as(
        "SELECT to_char(d, 'YYYY-MM-DD'), to_char(d + 1, 'YYYY-MM-DD')
         FROM (SELECT (now() AT TIME ZONE 'Europe/Amsterdam')::date AS d) x",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let day = vol(json!({"from": today.0, "to": today.1})).await.unwrap();
    assert_eq!(day["muscle_groups"].group("chest")["sets"], 6.0);
    let midnight: DateTime<Utc> = sqlx::query_scalar("SELECT date_trunc('day', now(), 'Europe/Amsterdam')")
        .fetch_one(&pool)
        .await
        .unwrap();
    let from = DateTime::parse_from_rfc3339(day["from"].as_str().unwrap()).unwrap();
    assert_eq!(from.with_timezone(&Utc), midnight);
    let before = vol(json!({"from": "2026-01-01", "to": today.0})).await.unwrap();
    assert_eq!(before["muscle_groups"].group("chest")["sets"], 0.0);
}

#[sqlx::test]
async fn settings_profile_round_trip(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let s = setup::settings_update(
        &app,
        ME,
        a(json!({"goal": "hypertrophy", "goals_notes": " Hyrox on 14 March ",
                 "priority_muscle_groups": ["back", "lats", "calves"],
                 "limitations": "left shoulder: no overhead pressing"})),
    )
    .await
    .unwrap();
    assert_eq!(s["goals_notes"], "Hyrox on 14 March");
    assert_eq!(
        s["priority_muscle_groups"],
        json!(["lats", "upper_back", "lower_back", "calves"])
    );
    assert_eq!(s["limitations"], "left shoulder: no overhead pressing");

    let got = setup::settings_get(&app, ME).await.unwrap();
    let ov = search::training_overview(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    for k in ["goal", "goals_notes", "priority_muscle_groups", "limitations"] {
        assert_eq!(got[k], s[k], "settings_get {k}");
        assert_eq!(ov["overview"][k], s[k], "overview {k}");
    }

    let s = setup::settings_update(&app, ME, a(json!({"limitations": "none"})))
        .await
        .unwrap();
    assert_eq!(s["limitations"], "none");
    assert_eq!(s["goals_notes"], "Hyrox on 14 March", "partial update keeps the rest");
    assert_eq!(s["goal"], "hypertrophy");

    let s = setup::settings_update(&app, ME, a(json!({"goals_notes": "", "priority_muscle_groups": []})))
        .await
        .unwrap();
    assert!(
        s["goals_notes"].is_null() && s["priority_muscle_groups"].is_null(),
        "{s}"
    );
    assert_eq!(s["limitations"], "none");

    let bad = user_msg(setup::settings_update(&app, ME, a(json!({"priority_muscle_groups": ["pecs"]}))).await);
    assert!(bad.contains("unknown muscle group"), "{bad}");
}

#[sqlx::test]
async fn measurements_round_trip(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let old = measurement::measurement_add(
        &app,
        ME,
        a(json!({"kind": "body_fat", "value": 20.0, "measured_at": hours_ago(30 * 24)})),
    )
    .await
    .unwrap();
    assert_eq!(old["kind"], "body_fat");
    assert_eq!(old["unit"], "%");
    let new = measurement::measurement_add(
        &app,
        ME,
        a(json!({"kind": "body_fat", "value": 18.5, "notes": "morning"})),
    )
    .await
    .unwrap();

    let list = measurement::measurement_list(&app, ME, a(json!({"kind": "body_fat"})))
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["id"], new["id"], "newest first");
    let recent = measurement::measurement_list(&app, ME, a(json!({"kind": "body_fat", "from": hours_ago(24)})))
        .await
        .unwrap();
    assert_eq!(recent.as_array().unwrap().len(), 1);

    let up = measurement::measurement_update(
        &app,
        ME,
        a(json!({"id": new["id"], "value": 18.0, "notes": "after breakfast"})),
    )
    .await
    .unwrap();
    assert_eq!(up["value"], 18.0);
    assert_eq!(up["notes"], "after breakfast");

    // owner isolation
    let id = new["id"].clone();
    assert_eq!(
        measurement::measurement_list(&app, "bob", a(json!({"kind": "body_fat"})))
            .await
            .unwrap(),
        json!([])
    );
    let msg = user_msg(measurement::measurement_update(&app, "bob", a(json!({"id": id, "value": 1.0}))).await);
    assert!(msg.contains("no measurement"), "{msg}");
    let msg = user_msg(measurement::measurement_delete(&app, "bob", a(json!({"id": id}))).await);
    assert!(msg.contains("no measurement"), "{msg}");

    measurement::measurement_delete(&app, ME, a(json!({"id": id})))
        .await
        .unwrap();
    let list = measurement::measurement_list(&app, ME, a(json!({"kind": "body_fat"})))
        .await
        .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);

    let future = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
    user_msg(
        measurement::measurement_add(
            &app,
            ME,
            a(json!({"kind": "body_fat", "value": 18.0, "measured_at": future})),
        )
        .await,
    );
    let msg = user_msg(measurement::measurement_add(&app, ME, a(json!({"kind": "body_fat", "value": 0.0}))).await);
    assert!(msg.contains("invalid value"), "{msg}");
}

#[sqlx::test]
async fn vitals_store_their_unit_and_show_in_overview(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let add = |v: Value| {
        let app = app.clone();
        async move { measurement::measurement_add(&app, ME, a(v)).await }
    };
    for (kind, value, unit) in [
        ("body_fat", 18.5, "%"),
        ("blood_pressure_systolic", 128.0, "mmHg"),
        ("blood_pressure_diastolic", 82.0, "mmHg"),
    ] {
        let m = add(json!({"kind": kind, "value": value})).await.unwrap();
        assert_eq!((m["kind"].as_str(), m["unit"].as_str()), (Some(kind), Some(unit)));
    }
    add(json!({"kind": "blood_pressure_systolic", "value": 120, "measured_at": hours_ago(48)}))
        .await
        .unwrap();

    let msg = user_msg(add(json!({"kind": "body_fat", "value": 120})).await);
    assert!(msg.contains("invalid value"), "{msg}");
    let msg = user_msg(add(json!({"kind": "blood_pressure_systolic", "value": 128, "unit": "percent"})).await);
    assert!(msg.contains("mmhg"), "{msg}");

    let ov = search::training_overview(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    let m = &ov["overview"]["measurements"];
    assert_eq!(
        m["blood_pressure_systolic"]["value"], 128.0,
        "latest, not the older 120: {m}"
    );
    assert_eq!(m["blood_pressure_systolic"]["avg_28d"], 124.0, "{m}");
    assert_eq!(m["body_fat"]["unit"], "%");
    assert!(
        ov["overview"].get("body_weight").is_none(),
        "body weight is no longer tracked"
    );
}

#[test]
fn removed_kinds_are_rejected() {
    for kind in [
        "body_weight",
        "body_height",
        "waist_circumference",
        "resting_heart_rate",
    ] {
        assert!(
            serde_json::from_value::<measurement::MeasurementAddArgs>(json!({"kind": kind, "value": 80})).is_err(),
            "{kind}"
        );
    }
    assert!(
        serde_json::from_value::<measurement::MeasurementAddArgs>(json!({"value": 18.5})).is_err(),
        "kind is required"
    );
}

#[sqlx::test]
async fn only_working_sets_count(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    let added = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "sets": [
            {"reps": 10, "weight_kg": 60, "set_type": "warmup"},
            {"reps": 10, "weight_kg": 100}, {"reps": 10, "weight_kg": 100, "set_type": "working"},
            {"reps": 3, "weight_kg": 160, "set_type": "interrupted"}
        ]})),
    )
    .await
    .unwrap();
    let sets = added["workout"]["exercises"][0]["sets"].as_array().unwrap();
    let types: Vec<_> = sets.iter().map(|s| s["set_type"].clone()).collect();
    assert_eq!(
        types,
        [
            json!("warmup"),
            json!("working"),
            json!("working"),
            json!("interrupted")
        ]
    );
    let cut_short = sets[3]["set_id"].clone();

    let quads = |app: App| async move {
        search::muscle_group_volume(&app, ME, a(json!({"days": 7})))
            .await
            .unwrap()["muscle_groups"]
            .group("quads")["sets"]
            .clone()
    };
    let heaviest = |app: App| async move {
        search::personal_records(&app, ME, a(json!({"exercise": "leg press"})))
            .await
            .unwrap()[0]["heaviest_set"]["weight_kg"]
            .clone()
    };
    assert_eq!(quads(app.clone()).await, 2.0);
    assert_eq!(heaviest(app.clone()).await, 100.0);
    let top = search::progress(&app, ME, a(json!({"exercise": "leg press", "metric": "top_weight"})))
        .await
        .unwrap();
    assert_eq!(top["series"][0]["value"], 100.0);
    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    let lp = ov["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Leg press")
        .unwrap();
    assert_eq!(lp["last_session"]["sets"].as_array().unwrap().len(), 2);
    assert_eq!(ov["overview"]["done_today"][0]["sets"], 2);
    assert_eq!(ov["overview"]["muscle_groups"].group("quads")["sets_7d"], 2.0);

    let warmups = search::set_search(&app, ME, a(json!({"set_type": "warmup"})))
        .await
        .unwrap();
    assert_eq!(warmups.as_array().unwrap().len(), 1);
    assert_eq!(warmups[0]["weight_kg"], 60.0);

    workout::set_update(&app, ME, a(json!({"set_id": cut_short, "set_type": "working"})))
        .await
        .unwrap();
    assert_eq!(quads(app.clone()).await, 3.0);
    assert_eq!(heaviest(app.clone()).await, 160.0);

    // cardio minutes count every set, warm-ups included
    setup::exercise_create(&app, ME, a(json!({"name": "Bike", "kind": "cardio"})))
        .await
        .unwrap();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "bike", "sets": [{"duration_s": 600, "distance_m": 4000, "set_type": "warmup"}]})),
    )
    .await
    .unwrap();
    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(
        ov["overview"]["cardio_this_week"],
        json!({"minutes": 10, "distance_km": 4.0})
    );
}

#[sqlx::test]
async fn unilateral_counts_per_side(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    let chest = |app: App| async move {
        search::muscle_group_volume(&app, ME, a(json!({"days": 7})))
            .await
            .unwrap()["muscle_groups"]
            .group("chest")["sets"]
            .clone()
    };
    let add = |app: App, extra: Value| async move {
        let mut args = json!({"exercise": "cable fly", "sets": [{"reps": 10, "weight_kg": 20}]});
        args.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        workout::set_add(&app, ME, a(args)).await.unwrap()
    };
    let newest = |app: App| async move {
        search::set_search(&app, ME, a(json!({"exercise": "cable fly", "limit": 1})))
            .await
            .unwrap()[0]
            .clone()
    };

    add(app.clone(), json!({})).await;
    assert_eq!(chest(app.clone()).await, 1.0);
    assert_eq!(newest(app.clone()).await["volume_kg"], 200.0);

    // 10 per side at 20 kg: twice the work of the bilateral set, same per-limb e1RM.
    add(app.clone(), json!({"unilateral": true})).await;
    assert_eq!(chest(app.clone()).await, 3.0);
    let uni = newest(app.clone()).await;
    assert_eq!(uni["unilateral"], true);
    assert_eq!(uni["volume_kg"], 400.0);
    assert_eq!(uni["e1rm_kg"], 26.7);

    // Logged as 40 kg for both sides together: the same work.
    let added = add(
        app.clone(),
        json!({"unilateral": true, "load_basis": "total", "sets": [{"reps": 10, "weight_kg": 40}]}),
    )
    .await;
    let total = newest(app.clone()).await;
    assert_eq!(total["load_basis"], "total");
    assert_eq!(total["volume_kg"], 400.0);
    assert_eq!(total["e1rm_kg"], 26.7);
    assert_eq!(chest(app.clone()).await, 5.0);
    // 40 kg total and 20 kg per side are the same weight.
    let top = search::progress(&app, ME, a(json!({"exercise": "cable fly", "metric": "top_weight"})))
        .await
        .unwrap();
    assert_eq!(top["series"][0]["value"], 20.0);
    let pr = search::personal_records(&app, ME, a(json!({"exercise": "cable fly"})))
        .await
        .unwrap();
    assert_eq!(pr[0]["heaviest_set"]["weight_kg"], 20.0);
    assert_eq!(pr[0]["max_reps_by_weight_kg"], json!([{"weight_kg": 20, "reps": 10}]));

    let sets = added["workout"]["exercises"][0]["sets"].as_array().unwrap();
    // load_basis only means something on a unilateral set.
    let msg = user_msg(
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": "cable fly", "sets": [{"reps": 10, "weight_kg": 40}], "load_basis": "total"})),
        )
        .await,
    );
    assert!(msg.contains("unilateral"), "{msg}");
    let msg =
        user_msg(workout::set_update(&app, ME, a(json!({"set_id": sets[0]["set_id"], "load_basis": "total"}))).await);
    assert!(msg.contains("unilateral"), "{msg}");
    // Back to the default convention: 40 kg per side.
    workout::set_update(
        &app,
        ME,
        a(json!({"set_id": sets[2]["set_id"], "load_basis": "default"})),
    )
    .await
    .unwrap();
    let cleared = newest(app.clone()).await;
    assert_eq!(cleared["load_basis"], Value::Null);
    assert_eq!(cleared["volume_kg"], 800.0);
    workout::set_update(&app, ME, a(json!({"set_id": sets[2]["set_id"], "load_basis": "total"})))
        .await
        .unwrap();
    workout::set_update(&app, ME, a(json!({"set_id": sets[2]["set_id"], "unilateral": false})))
        .await
        .unwrap();
    assert_eq!(chest(app.clone()).await, 4.0);
    assert_eq!(newest(app.clone()).await["load_basis"], Value::Null);
    let sets_metric = search::progress(&app, ME, a(json!({"exercise": "cable fly", "metric": "sets"})))
        .await
        .unwrap();
    assert_eq!(sets_metric["series"][0]["value"], 4);
    // The best e1RM reports the per-side weight it was computed from.
    add(
        app.clone(),
        json!({"unilateral": true, "load_basis": "total", "sets": [{"reps": 10, "weight_kg": 120}]}),
    )
    .await;
    let pr = search::personal_records(&app, ME, a(json!({"exercise": "cable fly"})))
        .await
        .unwrap();
    assert_eq!(pr[0]["best_e1rm"]["weight_kg"], 60.0);
}

async fn past_fly(app: &App, days: i64, kg: f64, variant: Option<&str>) -> Value {
    let start = Utc::now() - chrono::Duration::days(days);
    let w = workout::add_past(
        app,
        ME,
        a(json!({"gym": "ndsm", "started_at": start.to_rfc3339(),
                 "finished_at": (start + chrono::Duration::hours(1)).to_rfc3339()})),
    )
    .await
    .unwrap();
    let mut args = json!({"exercise": "cable fly", "workout_id": w["workout_id"],
                          "sets": [{"reps": 10, "weight_kg": kg,
                                    "performed_at": (start + chrono::Duration::minutes(10)).to_rfc3339()}]});
    if let Some(v) = variant {
        args["variant"] = json!(v);
    }
    workout::set_add(app, ME, a(args)).await.unwrap()
}

#[sqlx::test]
async fn variant_defaults_per_gym_and_splits_history(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;

    let given = past_fly(&app, 4, 50.0, Some(" Wide grip ")).await;
    assert_eq!(given["variant"], "Wide grip");
    assert_eq!(given["variant_source"], "given");
    let inherited = past_fly(&app, 3, 55.0, None).await;
    assert_eq!(inherited["variant"], "Wide grip");
    assert_eq!(inherited["variant_source"], "last_used_here");
    let standard = past_fly(&app, 2, 70.0, Some("")).await;
    assert!(standard["variant"].is_null());
    assert_eq!(standard["variant_source"], "none");
    past_fly(&app, 1, 60.0, Some("neutral grip")).await;

    let rec = search::personal_records(&app, ME, a(json!({"exercise": "cable fly"})))
        .await
        .unwrap();
    let by_variant: Vec<(Value, Value)> = rec
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["variant"].clone(), r["heaviest_set"]["weight_kg"].clone()))
        .collect();
    assert_eq!(
        by_variant,
        [
            (Value::Null, json!(70)),
            (json!("neutral grip"), json!(60)),
            (json!("Wide grip"), json!(55))
        ]
    );

    let series = search::progress(
        &app,
        ME,
        a(json!({"exercise": "cable fly", "metric": "top_weight", "bucket": "day"})),
    )
    .await
    .unwrap();
    let variants: Vec<Value> = series["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["variant"].clone())
        .collect();
    assert_eq!(variants.iter().filter(|v| **v == "Wide grip").count(), 2);
    assert_eq!(variants.len(), 4);
    let neutral = search::set_search(&app, ME, a(json!({"variant": "neutral"})))
        .await
        .unwrap();
    assert_eq!(neutral.as_array().unwrap().len(), 1);
    assert_eq!(neutral[0]["weight_kg"], 60.0);

    // The last variant here is neutral, so last_session is the neutral session, not the heavier standard one.
    let ov = search::training_overview(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    let fly = ov["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Cable fly")
        .unwrap();
    assert_eq!(fly["variant_here"], "neutral grip");
    assert_eq!(fly["last_session"]["sets"][0]["weight_kg"], 60.0);
    assert_eq!(fly["variants_used"], json!(["neutral grip", "Wide grip"]));

    // Never done at Basic-Fit, so last_session is the newest session anywhere, grip and all.
    let ov = search::training_overview(&app, ME, a(json!({"gym": "basic-fit"})))
        .await
        .unwrap();
    let fly = ov["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Cable fly")
        .unwrap();
    assert!(fly["variant_here"].is_null());
    assert_eq!(fly["last_session"]["sets"][0]["weight_kg"], 60.0);

    let set_id = neutral[0]["set_id"].clone();
    let w = workout::set_update(&app, ME, a(json!({"set_id": set_id, "variant": ""})))
        .await
        .unwrap();
    assert!(w["exercises"][0]["sets"][0]["variant"].is_null());
}

#[sqlx::test]
async fn standard_variant_is_remembered_after_a_grip(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;

    past_fly(&app, 3, 50.0, Some("Wide grip")).await;
    past_fly(&app, 2, 40.0, Some("")).await;
    // The last session here was standard, so the overview must not fall back to the wide-grip day.
    let ov = search::training_overview(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    let fly = ov["overview"]["exercises"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "Cable fly")
        .unwrap();
    assert!(fly["variant_here"].is_null());
    assert_eq!(fly["last_session"]["sets"][0]["weight_kg"], 40.0);

    let inherited = past_fly(&app, 1, 45.0, None).await;
    assert!(inherited["variant"].is_null());
    assert_eq!(inherited["variant_source"], "last_used_here");
}

#[sqlx::test]
async fn batch_update_and_delete_are_all_or_nothing(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    let added = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "cable fly", "sets": [{"reps": 10, "weight_kg": 20}, {"reps": 10, "weight_kg": 20}, {"reps": 8, "weight_kg": 20}]})),
    )
    .await
    .unwrap();
    let ids: Vec<i64> = added["workout"]["exercises"][0]["sets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["set_id"].as_i64().unwrap())
        .collect();
    let sets = |w: &Value| w["exercises"][0]["sets"].as_array().unwrap().clone();

    // set_ids always returns {"workouts": [...]}, even when the sets share one workout.
    let w = &workout::set_update(
        &app,
        ME,
        a(json!({"set_ids": ids, "weight_kg": 22.5, "variant": "vertical handles"})),
    )
    .await
    .unwrap()["workouts"][0];
    assert!(
        sets(w)
            .iter()
            .all(|s| s["weight_kg"] == 22.5 && s["variant"] == "vertical handles")
    );

    // One bad entry rejects the batch, names the entry, and changes nothing.
    let msg = user_msg(workout::set_update(&app, ME, a(json!({"set_ids": [ids[0], 999999], "weight_kg": 30}))).await);
    assert!(msg.contains("set 999999"), "{msg}");
    setup::gym_create(&app, "bob", a(json!({"name": "Bob's gym"})))
        .await
        .unwrap();
    setup::exercise_create(&app, "bob", a(json!({"name": "Squat", "muscle_groups": ["quads"]})))
        .await
        .unwrap();
    workout::start(&app, "bob", a(json!({"gym": "bob's gym"})))
        .await
        .unwrap();
    let bobs = workout::set_add(
        &app,
        "bob",
        a(json!({"exercise": "squat", "sets": [{"reps": 5, "weight_kg": 100}]})),
    )
    .await
    .unwrap()["workout"]["exercises"][0]["sets"][0]["set_id"]
        .clone();
    let msg = user_msg(workout::set_delete(&app, ME, a(json!({"set_ids": [ids[0], bobs]}))).await);
    assert!(msg.contains(&format!("set {bobs}")), "{msg}");
    let msg = user_msg(workout::set_update(&app, ME, a(json!({"set_ids": ids, "rpe": 11}))).await);
    assert!(msg.starts_with(&format!("set {}", ids[0])), "{msg}");
    let w = workout::current(&app, ME).await.unwrap()["open_workout"].clone();
    assert_eq!(sets(&w).len(), 3);
    assert!(sets(&w).iter().all(|s| s["weight_kg"] == 22.5 && s["rpe"].is_null()));

    let msg = user_msg(workout::set_delete(&app, ME, a(json!({"set_id": ids[0], "set_ids": [ids[1]]}))).await);
    assert!(msg.contains("not both"), "{msg}");
    let msg = user_msg(workout::set_delete(&app, ME, a(json!({}))).await);
    assert_eq!(msg, "pass set_id or set_ids");
    // The 1-100 bound runs before any lookup; 100 unknown ids pass it and fail on the first id.
    let msg = user_msg(workout::set_delete(&app, ME, a(json!({"set_ids": []}))).await);
    assert_eq!(msg, "pass between 1 and 100 set_ids");
    let many: Vec<i64> = (900_001..=900_101).collect();
    let msg = user_msg(workout::set_update(&app, ME, a(json!({"set_ids": many, "weight_kg": 30}))).await);
    assert_eq!(msg, "pass between 1 and 100 set_ids");
    let msg = user_msg(workout::set_delete(&app, ME, a(json!({"set_ids": many[..100]}))).await);
    assert!(msg.starts_with("set 900001"), "{msg}");
    let msg = user_msg(
        workout::set_update(
            &app,
            ME,
            a(json!({"set_ids": [ids[0], ids[1]], "performed_at": hours_ago(1)})),
        )
        .await,
    );
    assert!(msg.contains("one set at a time"), "{msg}");
    // A single id in set_ids may still move.
    workout::set_update(&app, ME, a(json!({"set_ids": [ids[2]], "performed_at": hours_ago(1)})))
        .await
        .unwrap();
    let w = workout::set_delete(&app, ME, a(json!({"set_ids": [ids[0], ids[1], ids[0]]})))
        .await
        .unwrap();
    assert_eq!(sets(&w["workouts"][0]).len(), 1);
    let w = workout::set_delete(&app, ME, a(json!({"set_id": ids[2]})))
        .await
        .unwrap();
    assert!(w["workouts"].is_null() && w["workout_id"].is_number(), "{w}");
}

#[sqlx::test]
async fn cardio_metrics_round_trip(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::exercise_create(&app, ME, a(json!({"name": "Treadmill run", "kind": "cardio"})))
        .await
        .unwrap();
    workout::start(&app, ME, a(json!({"gym": "ndsm", "started_at": hours_ago(1)})))
        .await
        .unwrap();
    let added = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "treadmill", "sets": [
            {"duration_s": 1800, "distance_m": 5000, "avg_hr": 148, "max_hr": 171, "calories": 320,
             "avg_incline_pct": 2, "max_incline_pct": 6, "avg_speed": 6, "max_speed": 7.5, "speed_unit": "mph"},
            {"duration_s": 600, "avg_hr": 120, "avg_speed": 10}]})),
    )
    .await
    .unwrap();
    let sets = added["workout"]["exercises"][0]["sets"].as_array().unwrap().clone();
    assert_eq!(sets[0]["avg_hr"], 148);
    assert_eq!(sets[0]["calories"], 320);
    assert_eq!(sets[0]["max_incline_pct"], 6.0);
    assert!((sets[0]["avg_speed_kmh"].as_f64().unwrap() - 9.656064).abs() < 1e-6);
    assert!((sets[0]["max_speed_kmh"].as_f64().unwrap() - 12.07008).abs() < 1e-6);
    assert_eq!(sets[1]["avg_speed_kmh"], 10.0);

    let metric = |m: &str| search::progress(&app, ME, a(json!({"exercise": "treadmill", "metric": m})));
    assert_eq!(metric("avg_hr").await.unwrap()["series"][0]["value"], 141);
    assert_eq!(metric("avg_speed").await.unwrap()["series"][0]["value"], 9.7);
    assert_eq!(metric("calories").await.unwrap()["series"][0]["value"], 320);
    let found = search::set_search(
        &app,
        ME,
        a(json!({"exercise": "treadmill", "order_by": "distance", "limit": 1})),
    )
    .await
    .unwrap();
    assert_eq!(found[0]["max_hr"], 171);

    let id = sets[1]["set_id"].clone();
    let msg = user_msg(workout::set_update(&app, ME, a(json!({"set_id": id, "max_hr": 110}))).await);
    assert!(msg.contains("workout_set_hr_order"), "{msg}");
    let msg = user_msg(workout::set_update(&app, ME, a(json!({"set_id": id, "avg_hr": 300}))).await);
    assert!(msg.contains("invalid value"), "{msg}");
    let w = workout::set_update(&app, ME, a(json!({"set_id": id, "calories": 90})))
        .await
        .unwrap();
    assert_eq!(w["exercises"][0]["sets"][1]["calories"], 90);
}

#[sqlx::test]
async fn shift_sets_moves_workout_with_its_sets(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let start = (Utc::now() - chrono::Duration::days(3))
        .duration_trunc(chrono::Duration::minutes(1))
        .unwrap();
    let at = |m: i64| (start + chrono::Duration::minutes(m)).to_rfc3339();
    let w = workout::add_past(
        &app,
        ME,
        a(json!({"gym": "ndsm", "started_at": at(0), "finished_at": at(60)})),
    )
    .await
    .unwrap();
    let id = w["workout_id"].clone();
    workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "workout_id": id,
                 "sets": [{"reps": 10, "weight_kg": 100, "performed_at": at(10)},
                          {"reps": 10, "weight_kg": 100, "performed_at": at(25)}]})),
    )
    .await
    .unwrap();
    let times = |w: &Value| -> Vec<DateTime<Utc>> {
        w["exercises"][0]["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| serde_json::from_value(s["performed_at"].clone()).unwrap())
            .collect()
    };
    let ts = |v: &Value| -> DateTime<Utc> { serde_json::from_value(v.clone()).unwrap() };

    // Without the flag, the first set would fall before the new start.
    let msg = user_msg(workout::update(&app, ME, a(json!({"workout_id": id, "started_at": at(15)}))).await);
    assert!(msg.contains("adjust those sets first"), "{msg}");
    let msg = user_msg(workout::update(&app, ME, a(json!({"workout_id": id, "shift_sets": true}))).await);
    assert!(msg.contains("needs started_at"), "{msg}");

    let moved = workout::update(
        &app,
        ME,
        a(json!({"workout_id": id, "started_at": at(120), "shift_sets": true})),
    )
    .await
    .unwrap();
    let m = |x: i64| start + chrono::Duration::minutes(x);
    assert_eq!(times(&moved), [m(130), m(145)]);
    assert_eq!(ts(&moved["started_at"]), m(120));
    assert_eq!(ts(&moved["finished_at"]), m(180));

    // An explicit finish before the shifted sets is rejected, and nothing moves.
    let msg = user_msg(
        workout::update(
            &app,
            ME,
            a(json!({"workout_id": id, "started_at": at(0), "finished_at": at(5), "shift_sets": true})),
        )
        .await,
    );
    assert!(msg.contains("outside"), "{msg}");
    let msg = user_msg(
        workout::update(
            &app,
            ME,
            a(
                json!({"workout_id": id, "started_at": (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
                     "shift_sets": true}),
            ),
        )
        .await,
    );
    assert!(msg.contains("future"), "{msg}");
    // The sets land in the past, but the shifted finish would not.
    let msg = user_msg(
        workout::update(
            &app,
            ME,
            a(
                json!({"workout_id": id, "started_at": (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339(),
                     "shift_sets": true}),
            ),
        )
        .await,
    );
    assert!(msg.contains("future"), "{msg}");
    let w = workout::update(&app, ME, a(json!({"workout_id": id}))).await.unwrap();
    assert_eq!(times(&w), [m(130), m(145)]);
    assert_eq!(ts(&w["finished_at"]), m(180));
}

#[sqlx::test]
async fn estimated_times_clear_on_a_real_time(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let start = Utc::now() - chrono::Duration::days(2);
    let at = |m: i64| (start + chrono::Duration::minutes(m)).to_rfc3339();
    let w = workout::add_past(
        &app,
        ME,
        a(json!({"gym": "ndsm", "started_at": at(0), "finished_at": at(90), "time_estimated": true})),
    )
    .await
    .unwrap();
    assert_eq!(w["time_estimated"], true);
    let id = w["workout_id"].clone();
    let added = workout::set_add(
        &app,
        ME,
        a(json!({"exercise": "leg press", "workout_id": id, "sets": [
            {"reps": 10, "weight_kg": 100, "performed_at": at(30), "time_estimated": true},
            {"reps": 10, "weight_kg": 100, "performed_at": at(40)}]})),
    )
    .await
    .unwrap();
    let flags = |w: &Value| -> Vec<Value> {
        w["exercises"][0]["sets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["time_estimated"].clone())
            .collect()
    };
    assert_eq!(flags(&added["workout"]), [json!(true), json!(false)]);
    let guess = added["workout"]["exercises"][0]["sets"][0]["set_id"].clone();

    let upd = |args: Value| workout::set_update(&app, ME, a(args));
    let w = upd(json!({"set_id": guess, "notes": "felt heavy"})).await.unwrap();
    assert_eq!(flags(&w)[0], true);
    let w = upd(json!({"set_id": guess, "performed_at": at(20), "time_estimated": true}))
        .await
        .unwrap();
    assert_eq!(flags(&w)[0], true);
    let w = upd(json!({"set_id": guess, "performed_at": at(22)})).await.unwrap();
    assert_eq!(flags(&w)[0], false);
    let found = search::set_search(&app, ME, a(json!({"exercise": "leg press", "limit": 1})))
        .await
        .unwrap();
    assert_eq!(found[0]["time_estimated"], false);

    // Moving the session keeps the sets' own flags; a new start clears the workout's flag.
    upd(json!({"set_id": guess, "time_estimated": true})).await.unwrap();
    let w = workout::update(&app, ME, a(json!({"workout_id": id, "notes": "legs"})))
        .await
        .unwrap();
    assert_eq!(w["time_estimated"], true);
    let w = workout::update(
        &app,
        ME,
        a(json!({"workout_id": id, "started_at": at(10), "shift_sets": true})),
    )
    .await
    .unwrap();
    assert_eq!(w["time_estimated"], false);
    assert_eq!(flags(&w), [json!(true), json!(false)]);
}

#[sqlx::test]
async fn body_notes_round_trip(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let w = workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let wid = w["workout"]["workout_id"].as_i64().unwrap();
    let old = body_note::body_note_add(
        &app,
        ME,
        a(json!({"location": "Left knee ", "severity": 3, "noted_at": hours_ago(20 * 24)})),
    )
    .await
    .unwrap();
    assert_eq!(old["location"], "left knee");
    let new = body_note::body_note_add(
        &app,
        ME,
        a(json!({"location": "lower_back", "notes": "tired after deadlifts", "workout_id": wid})),
    )
    .await
    .unwrap();
    assert_eq!(new["workout_id"], wid);
    assert!(new["severity"].is_null());
    let id = new["id"].as_i64().unwrap();

    let list = body_note::body_note_list(&app, ME, a(json!({}))).await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert_eq!(list[0]["id"], id, "newest first");
    let knee = body_note::body_note_list(&app, ME, a(json!({"location": "KNEE"})))
        .await
        .unwrap();
    assert_eq!(knee.as_array().unwrap().len(), 1);

    let up = body_note::body_note_update(&app, ME, a(json!({"id": id, "severity": 6})))
        .await
        .unwrap();
    assert_eq!(up["severity"], 6);
    assert_eq!(up["notes"], "tired after deadlifts", "unset fields stay");
    let msg = user_msg(body_note::body_note_update(&app, ME, a(json!({"id": id, "severity": 11}))).await);
    assert!(msg.contains("invalid value"), "{msg}");
    let cleared = body_note::body_note_update(&app, ME, a(json!({"id": id, "severity": 0, "workout_id": 0})))
        .await
        .unwrap();
    assert!(cleared["severity"].is_null(), "0 clears severity");
    assert!(cleared["workout_id"].is_null(), "0 unlinks the workout");

    let o = search::training_overview(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    let recent = o["overview"]["body_notes"].as_array().unwrap();
    assert_eq!(recent.len(), 1, "only the last 14 days");
    assert_eq!(recent[0]["location"], "lower_back");

    assert_eq!(
        body_note::body_note_list(&app, "bob", a(json!({}))).await.unwrap(),
        json!([])
    );
    let msg = user_msg(body_note::body_note_update(&app, "bob", a(json!({"id": id, "severity": 1}))).await);
    assert!(msg.contains("no body note"), "{msg}");
    let msg = user_msg(body_note::body_note_delete(&app, "bob", a(json!({"id": id}))).await);
    assert!(msg.contains("no body note"), "{msg}");
    let msg = user_msg(body_note::body_note_add(&app, "bob", a(json!({"location": "neck", "workout_id": wid}))).await);
    assert!(msg.contains("no workout"), "{msg}");

    let d = body_note::body_note_delete(&app, ME, a(json!({"id": id})))
        .await
        .unwrap();
    assert_eq!(d["deleted"]["id"], id);
    assert_eq!(
        body_note::body_note_list(&app, ME, a(json!({})))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test]
async fn infinite_times_are_user_errors(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    workout::start(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let msg = user_msg(
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": "leg press", "sets": [{"reps": 5, "performed_at": "-infinity"}]})),
        )
        .await,
    );
    assert!(msg.contains("infinity"), "{msg}");
    let msg = user_msg(
        measurement::measurement_add(
            &app,
            ME,
            a(json!({"kind": "body_fat", "value": 18.0, "measured_at": "infinity"})),
        )
        .await,
    );
    assert!(msg.contains("infinity"), "{msg}");
    let msg = user_msg(search::muscle_group_volume(&app, ME, a(json!({"from": "-infinity"}))).await);
    assert!(msg.contains("infinity"), "{msg}");
    // Postgres goes to year 294276, chrono only to 262143.
    let msg = user_msg(
        workout::set_add(
            &app,
            ME,
            a(json!({"exercise": "leg press", "sets": [{"reps": 5, "performed_at": "270000-01-01"}]})),
        )
        .await,
    );
    assert!(msg.contains("real date"), "{msg}");
    let msg =
        user_msg(search::muscle_group_volume(&app, ME, a(json!({"from": "2026-01-01", "to": "270000-01-01"}))).await);
    assert!(msg.contains("real date"), "{msg}");
    // Postgres' earliest time still fits chrono.
    let v = search::muscle_group_volume(&app, ME, a(json!({"from": "4714-11-25 BC"})))
        .await
        .unwrap();
    assert!(v.is_object(), "{v}");
}

/// `/api/v1/tools`: the MCP tools as plain JSON over HTTP, for apps.
#[sqlx::test]
async fn api_serves_the_tools_over_http(pool: PgPool) {
    use gym_mcp::{api, auth::Owner};
    let router = api::router(app(pool)).layer(axum::middleware::from_fn(
        |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(Owner(ME.into()));
            next.run(req).await
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/tools", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::new();
    let post = |name: &str, body: &'static str| {
        http.post(format!("{url}/{name}"))
            .header("content-type", "application/json; charset=utf-8")
            .body(body)
            .send()
    };

    let tools: Value = http.get(&url).send().await.unwrap().json().await.unwrap();
    assert!(
        tools
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "set_add" && t["inputSchema"].is_object())
    );

    let created = post("gym_create", r#"{"name": "Trainmore NDSM"}"#).await.unwrap();
    assert_eq!(created.status(), 200);
    let gyms: Value = post("gym_list", "").await.unwrap().json().await.unwrap();
    assert_eq!(gyms[0]["name"], "Trainmore NDSM", "{gyms}");

    let bad = post("gym_create", r#"{"nmae": 1}"#).await.unwrap();
    assert_eq!(bad.status(), 400);
    let err: Value = bad.json().await.unwrap();
    assert!(err["error"].as_str().unwrap().starts_with("invalid arguments"), "{err}");
    assert_eq!(post("gym_get", "{not json").await.unwrap().status(), 400);
    assert_eq!(post("nope", "").await.unwrap().status(), 404);
    // A cross-site "simple request" must not run a tool.
    let plain = http
        .post(format!("{url}/gym_create"))
        .header("content-type", "text/plain")
        .body(r#"{"name": "Evil"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(plain.status(), 415);
    let bare = http.post(format!("{url}/workout_finish")).send().await.unwrap();
    assert_eq!(bare.status(), 415);
}
