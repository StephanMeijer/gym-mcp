//! `GET /api/v1/tools`, `POST /api/v1/tools/{name}`: the MCP tools for apps, as plain JSON over HTTP.

use axum::body::Bytes;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::auth::Owner;
use crate::db::{App, AppError};
use crate::tools::{GymServer, call_api};

async fn list_tools() -> Response {
    Json(GymServer::tool_list()).into_response()
}

/// The JSON body is the tool's arguments (empty means `{}`); the result is the tool's output.
async fn call_tool(
    State(app): State<App>,
    Extension(Owner(owner)): Extension<Owner>,
    Path(name): Path<String>,
    body: Bytes,
) -> Response {
    let args = if body.is_empty() {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(a) => a,
            Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": e.to_string() }))).into_response(),
        }
    };
    match call_api(&app, &owner, &name, args).await {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(Ok(v)) => Json(v).into_response(),
        Some(Err(AppError::User(e))) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
        Some(Err(AppError::Db(e))) => {
            tracing::error!("database error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// The `/api` routes, without auth: callers add a layer that inserts [`Owner`].
pub fn router(app: App) -> Router {
    Router::new()
        .route("/v1/tools", get(list_tools))
        .route("/v1/tools/{name}", post(call_tool))
        .with_state(app)
}
