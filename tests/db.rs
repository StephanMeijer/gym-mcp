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
    assert_eq!(o["overview"]["muscle_groups"]["quads"]["days_since"], 0);
    assert!(o["overview"]["muscle_groups"]["chest"]["days_since"].is_null());
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
    assert_eq!(groups["chest"]["days_since"], 0);
    assert!(
        groups["triceps"]["days_since"].is_null(),
        "secondary doesn't count: {groups}"
    );
    assert!(groups.get("back").is_none() && groups.get("cardio").is_none());

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
    assert_eq!(g["chest"]["days_since"], 0);
    assert_eq!(g["chest"]["sets_7d"], 2.0);
    assert_eq!(g["chest"]["sets_28d"], 3.0);
    assert_eq!(g["chest"]["train_days_28d"], 2);
    assert_eq!(g["chest"]["avg_rpe_28d"], 7.5, "the set without rpe is ignored");
    assert_eq!(g["triceps"]["sets_7d"], 1.0, "secondary counts half");
    assert_eq!(g["triceps"]["sets_28d"], 1.5);
    assert!(g["triceps"]["days_since"].is_null(), "secondary only");
    assert_eq!(g["triceps"]["train_days_28d"], 0);
    assert_eq!(g["quads"]["sets_28d"], 0.0, "cardio doesn't count");
    assert!(g["calves"]["days_since"].is_null());
    assert_eq!(g["calves"]["sets_7d"], 0.0);
    assert!(g["calves"]["avg_rpe_28d"].is_null());
    assert_eq!(g.as_object().unwrap().len(), setup::MUSCLE_GROUPS.len());
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
    let chest = &d14["muscle_groups"]["chest"];
    assert_eq!(d14["days"], 14.0);
    assert_eq!(chest["sets"], 3.0);
    assert_eq!(chest["sets_per_week"], 1.5);
    assert_eq!(
        chest["from_exercises"],
        json!([{"exercise": "Barbell bench press", "sets": 3.0}])
    );
    assert_eq!(d14["muscle_groups"]["triceps"]["sets"], 1.5);
    assert_eq!(d14["muscle_groups"]["calves"]["from_exercises"], json!([]));

    let ranged =
        vol(json!({"from": (Utc::now() - chrono::Duration::days(14)).to_rfc3339(), "to": Utc::now().to_rfc3339()}))
            .await
            .unwrap();
    assert_eq!(ranged["muscle_groups"]["chest"]["sets"], 3.0);

    // A period that ends before today's sets: only the old set, but days_since is all-time.
    let old = vol(json!({"from": (Utc::now() - chrono::Duration::days(14)).to_rfc3339(),
                         "to": (Utc::now() - chrono::Duration::days(5)).to_rfc3339()}))
    .await
    .unwrap();
    assert_eq!(old["muscle_groups"]["chest"]["sets"], 1.0);
    assert_eq!(old["muscle_groups"]["chest"]["avg_rpe"], 7.0);
    assert_eq!(old["muscle_groups"]["chest"]["days_since"], 0);

    // The overview uses the same numbers.
    let ov = search::training_overview(&app, ME, a(json!({}))).await.unwrap();
    let og = &ov["overview"]["muscle_groups"]["chest"];
    let d7 = vol(json!({})).await.unwrap();
    let d28 = vol(json!({"days": 28})).await.unwrap();
    assert_eq!(og["sets_7d"], d7["muscle_groups"]["chest"]["sets"]);
    assert_eq!(og["sets_28d"], d28["muscle_groups"]["chest"]["sets"]);
    assert_eq!(og["train_days_28d"], d28["muscle_groups"]["chest"]["train_days"]);
    assert_eq!(og["avg_rpe_28d"], d28["muscle_groups"]["chest"]["avg_rpe"]);
    assert_eq!(d7["muscle_groups"]["chest"]["sets"], 2.0);

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
