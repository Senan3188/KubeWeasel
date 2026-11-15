use super::{service, validation::Change};
use crate::{error::Result, http::request::mutation, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::Value;
#[derive(Deserialize)]
struct Drain {
    #[serde(flatten)]
    change: Change,
    pod_uids: Vec<String>,
}
async fn list(State(s): State<AppState>) -> Result<Json<Value>> {
    Ok(Json(service::list(&s).await?))
}
async fn detail(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    Ok(Json(service::detail(&s, &name).await?))
}
async fn change(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Change>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    Ok(Json(service::change(&s, &name, &input).await?))
}
async fn preview(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    Ok(Json(service::preview(&s, &name).await?))
}
async fn drain(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Drain>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    Ok(Json(
        service::drain(&s, &name, &input.change, &input.pod_uids).await?,
    ))
}
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/nodes", get(list))
        .route("/api/nodes/{name}", get(detail).post(change))
        .route("/api/nodes/{name}/drain-preview", get(preview))
        .route("/api/nodes/{name}/drain", post(drain))
}
