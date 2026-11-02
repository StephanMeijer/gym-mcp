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
    setup::gym_create(app, ME, a(json!({"name": "Trainmore NDSM", "aliases": ["ndsm"]})))
        .await
        .unwrap();
    setup::gym_create(app, ME, a(json!({"name": "Basic-Fit Zuid"})))
        .await
        .unwrap();
    setup::gym_equipment_set(
        app,
        ME,
        a(json!({"gym": "ndsm", "items": [
            {"name": "leg press", "category": "machine", "max_load_kg": 300},
            {"name": "dumbbells", "category": "free_weight", "available_weights_kg": [2, 4, 6, 8, 10, 12, 14]},
            {"name": "cable tower", "status": "absent"}
        ]})),
    )
    .await
    .unwrap();
    for (name, groups, eq) in [
        ("Leg press", json!(["quads", "glutes"]), json!(["leg press"])),
        (
            "Dumbbell bench press",
            json!(["chest", "triceps"]),
            json!(["dumbbells", "flat bench"]),
        ),
        ("Cable fly", json!(["chest"]), json!(["cable tower"])),
        ("Push-up", json!(["chest", "triceps"]), json!([])),
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
async fn availability_distinguishes_unknown_from_absent(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    let by_name = |v: &Value, n: &str| v.as_array().unwrap().iter().find(|e| e["name"] == n).cloned();

    let r = setup::exercise_search(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    assert_eq!(by_name(&r, "Leg press").unwrap()["availability"], "available");
    assert_eq!(by_name(&r, "Push-up").unwrap()["availability"], "available");
    let bench = by_name(&r, "Dumbbell bench press").unwrap();
    assert_eq!(bench["availability"], "unverified");
    assert_eq!(bench["unknown_equipment"], json!(["flat bench"]));
    assert!(by_name(&r, "Cable fly").is_none(), "absent equipment hides it");

    let r = setup::exercise_search(&app, ME, a(json!({"gym": "ndsm", "include_unavailable": true})))
        .await
        .unwrap();
    assert_eq!(by_name(&r, "Cable fly").unwrap()["availability"], "unavailable");

    setup::gym_update(&app, ME, a(json!({"gym": "ndsm", "inventory_complete": true})))
        .await
        .unwrap();
    let r = setup::exercise_search(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    assert!(
        by_name(&r, "Dumbbell bench press").is_none(),
        "complete inventory: unknown = absent"
    );

    setup::gym_update(&app, ME, a(json!({"gym": "ndsm", "inventory_complete": false})))
        .await
        .unwrap();
    setup::gym_equipment_forget(&app, ME, a(json!({"gym": "ndsm", "names": ["cable tower"]})))
        .await
        .unwrap();
    let r = setup::exercise_search(&app, ME, a(json!({"gym": "ndsm"})))
        .await
        .unwrap();
    assert_eq!(by_name(&r, "Cable fly").unwrap()["availability"], "unverified");
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
    let names: Vec<_> = alt
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Dumbbell bench press"], "cable fly is absent at ndsm");
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
async fn equipment_has_a_description_separate_from_gym_notes(pool: PgPool) {
    let app = app(pool);
    seed(&app).await;
    setup::gym_equipment_set(
        &app,
        ME,
        a(json!({"gym": "ndsm", "items": [
            {"name": "leg press", "description": "45 degree plate-loaded sled", "notes": "two of them, upstairs"}
        ]})),
    )
    .await
    .unwrap();
    let gym = setup::gym_get(&app, ME, a(json!({"gym": "ndsm"}))).await.unwrap();
    let lp = gym["equipment_present"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "leg press")
        .unwrap()
        .clone();
    assert_eq!(lp["description"], "45 degree plate-loaded sled");
    assert_eq!(lp["notes"], "two of them, upstairs");

    // Updating the kind leaves the unset fields alone and is visible everywhere.
    let eq = setup::equipment_update(&app, ME, a(json!({"equipment": "legpress", "aliases": ["sled"]})))
        .await
        .unwrap();
    assert_eq!(eq["description"], "45 degree plate-loaded sled");
    assert_eq!(eq["aliases"], json!(["sled"]));
    let eq = setup::equipment_update(
        &app,
        ME,
        a(json!({"equipment": "sled", "description": "45° sled, plates on the side"})),
    )
    .await
    .unwrap();
    assert_eq!(eq["description"], "45° sled, plates on the side");
    let list = setup::equipment_list(&app, ME).await.unwrap();
    assert!(
        list.as_array()
            .unwrap()
            .iter()
            .any(|e| e["description"] == "45° sled, plates on the side")
    );

    let hits = search::search(&app, ME, a(json!({"query": "plates"}))).await.unwrap();
    assert_eq!(hits["notes"][0]["type"], "equipment");
    assert_eq!(hits["notes"][0]["name"], "leg press");

    // Exercise descriptions show up in search results too.
    setup::exercise_update(
        &app,
        ME,
        a(json!({"exercise": "push-up", "description": "hands under shoulders, chest to floor"})),
    )
    .await
    .unwrap();
    let ex = setup::exercise_search(&app, ME, a(json!({"query": "push-up"})))
        .await
        .unwrap();
    assert_eq!(ex[0]["description"], "hands under shoulders, chest to floor");
}
