//! Integration tests against a real Postgres. Needs DATABASE_URL (see README); each test gets a fresh database.

use std::time::Duration;

use chrono::Utc;
use gym_mcp::db::{App, AppError};
use gym_mcp::{search, setup, workout};
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
    assert_eq!(o["overview"]["days_since_muscle_group"]["quads"], 0);
    assert!(o["overview"]["days_since_muscle_group"]["chest"].is_null());
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
    assert!(user_msg(setup::settings_update(&app, ME, a(json!({}))).await).contains("timezone and/or goal"));

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
