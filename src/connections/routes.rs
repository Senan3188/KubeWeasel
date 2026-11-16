use super::{
    models::{Attachment, Input},
    service,
};
use crate::{error::Result, http::request::mutation, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{project}/connections",
            get(list).post(create),
        )
        .route(
            "/api/projects/{project}/connections/{id}",
            post(update).delete(remove),
        )
        .route("/api/apps/{app}/connections", get(app_list))
        .route(
            "/api/apps/{app}/connections/{id}",
            post(attach).delete(detach),
        )
        .route(
            "/api/projects/{project}/connections/{id}/restart",
            post(restart),
        )
}
async fn list(State(s): State<AppState>, Path(project): Path<String>) -> Result<Json<Value>> {
    Ok(Json(
        json!({"connections":service::catalog(&s,&project,None).await?}),
    ))
}
async fn app_list(State(s): State<AppState>, Path(app): Path<String>) -> Result<Json<Value>> {
    let project = service::app_project(&s, &app).await?;
    Ok(Json(
        json!({"connections":if let Some(p)=project {service::catalog(&s,&p,Some(&app)).await?}else{vec![]}}),
    ))
}
async fn create(
    State(s): State<AppState>,
    Path(project): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    Ok(Json(
        json!({"connection":service::save(&s,&project,None,input).await?}),
    ))
}
async fn update(
    State(s): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    Ok(Json(
        json!({"connection":service::save(&s,&project,Some(&id),input).await?}),
    ))
}
#[derive(Deserialize)]
struct Version {
    version: String,
}
async fn remove(
    State(s): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Version>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    service::remove(&s, &project, &id, &input.version).await?;
    Ok(Json(json!({"ok":true})))
}
async fn attach(
    State(s): State<AppState>,
    Path((app, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Attachment>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    service::attach(&s, &app, &id, input, false).await?;
    Ok(Json(json!({"ok":true})))
}
async fn detach(
    State(s): State<AppState>,
    Path((app, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Attachment>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    service::attach(&s, &app, &id, input, true).await?;
    Ok(Json(json!({"ok":true})))
}
async fn restart(
    State(s): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(input): Json<Version>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    Ok(Json(
        service::restart(&s, &project, &id, &input.version).await?,
    ))
}
