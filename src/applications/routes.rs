use super::{
    models::{Deploy, Scale},
    service,
};
use crate::{error::Result, http::request::mutation, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde_json::{Value, json};
pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/overview", get(overview))
        .route("/api/apps", post(deploy))
        .route("/api/apps/{name}/scale", post(scale))
        .route("/api/apps/{name}/restart", post(restart))
        .route("/api/apps/{name}/logs", get(logs))
}
async fn overview(State(s): State<AppState>) -> Result<Json<Value>> {
    Ok(Json(
        json!({"mode":if s.client.is_some(){"live"}else{"demo"},"namespace":s.namespace,"apps":service::overview(&s).await?}),
    ))
}
async fn deploy(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(d): Json<Deploy>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    service::deploy(&s, d).await?;
    Ok(Json(json!({"ok":true})))
}
async fn scale(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Scale>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    service::scale(&s, &name, body).await?;
    Ok(Json(json!({"ok":true})))
}
async fn restart(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    service::restart(&s, &name).await?;
    Ok(Json(json!({"ok":true})))
}
async fn logs(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    Ok(Json(json!({"logs":service::logs(&s,&name).await?})))
}
