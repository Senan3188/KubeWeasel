use super::{Cluster, Manager, connection, new_id, validate_name};
use crate::{
    error::{Result, invalid},
    http::{features, request::mutation},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tower::ServiceExt;
#[derive(Deserialize)]
struct Input {
    name: String,
    namespace: String,
    kubeconfig: String,
    #[serde(default)]
    context: String,
}
#[derive(Deserialize)]
struct Edit {
    name: String,
    #[serde(default)]
    kubeconfig: String,
    #[serde(default)]
    context: String,
}
async fn inspect(client: kube::Client, namespace: &str) -> Value {
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        connection::test(client, namespace),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            json!({"connected":false,"message":"The cluster connection or permission checks timed out. Check API reachability from the dashboard server.","permissions":[]})
        }
    }
}
async fn list(State(manager): State<Manager>) -> Result<Json<Value>> {
    Ok(Json(
        json!({"clusters":manager.store.list()?,"demo":manager.local.client.is_none()}),
    ))
}
async fn candidate(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate_name(&input.name)?;
    if manager.local.client.is_none() {
        return Err(invalid("Connecting clusters requires live mode."));
    }
    let (client, server) =
        connection::client(&input.kubeconfig, &input.context, &input.namespace).await?;
    let mut result = inspect(client, &input.namespace).await;
    result["server"] = json!(server);
    Ok(Json(result))
}
async fn create(
    State(manager): State<Manager>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate_name(&input.name)?;
    if manager.local.client.is_none() {
        return Err(invalid("Connecting clusters requires live mode."));
    }
    let _gate = manager.gate.lock().await;
    if manager.store.list()?.len() >= 20 {
        return Err(invalid("At most 20 cluster connections are supported."));
    }
    let (client, server) =
        connection::client(&input.kubeconfig, &input.context, &input.namespace).await?;
    let result = inspect(client, &input.namespace).await;
    if result["connected"] != true {
        return Err(invalid(
            result["message"]
                .as_str()
                .unwrap_or("Cannot reach cluster."),
        ));
    }
    let cluster = Cluster {
        id: new_id(),
        name: input.name.trim().into(),
        namespace: input.namespace,
        server,
        local: false,
        context: input.context,
    };
    manager.store.save(&cluster, Some(&input.kubeconfig))?;
    manager.resolve_locked(&cluster.id).await?;
    Ok(Json(json!({"cluster":cluster,"connection":result})))
}
async fn test(
    State(manager): State<Manager>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let state = manager.resolve(&id).await?;
    if let Some(client) = state.client {
        Ok(Json(inspect(client, &state.namespace).await))
    } else {
        Ok(Json(
            json!({"connected":false,"message":"Demo cluster does not contact Kubernetes.","permissions":[]}),
        ))
    }
}
async fn update(
    State(manager): State<Manager>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Edit>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate_name(&input.name)?;
    let _gate = manager.gate.lock().await;
    let mut cluster = manager.store.get(&id)?;
    cluster.name = input.name.trim().into();
    if !input.kubeconfig.is_empty() {
        if cluster.local {
            return Err(invalid(
                "The local cluster uses the server's Kubernetes configuration. Its credentials cannot be replaced here.",
            ));
        }
        let (client, server) =
            connection::client(&input.kubeconfig, &input.context, &cluster.namespace).await?;
        let result = inspect(client, &cluster.namespace).await;
        if result["connected"] != true {
            return Err(invalid(
                result["message"]
                    .as_str()
                    .unwrap_or("Cannot reach cluster."),
            ));
        }
        cluster.server = server;
        cluster.context = input.context;
        manager.store.save(&cluster, Some(&input.kubeconfig))?;
        manager.evict(&id).await;
        manager.resolve_locked(&id).await?;
    } else {
        manager.store.save(&cluster, None)?;
    }
    Ok(Json(json!({"cluster":cluster})))
}
async fn disconnect(
    State(manager): State<Manager>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = manager.gate.lock().await;
    let cluster = manager.store.get(&id)?;
    if cluster.local {
        return Err(invalid("The local cluster cannot be disconnected."));
    }
    manager.store.remove(&id)?;
    manager.evict(&id).await;
    Ok(Json(
        json!({"ok":true,"message":"Connection removed. Remote workloads, volumes and namespaces were not deleted. Project metadata is retained in the dashboard backup."}),
    ))
}
async fn dispatch(
    State(manager): State<Manager>,
    Path((id, _tail)): Path<(String, String)>,
    mut request: Request,
) -> Result<Response> {
    let (state, _lease) = manager.lease(&id).await?;
    let prefix = format!("/api/clusters/{id}/");
    let tail = request
        .uri()
        .path()
        .strip_prefix(&prefix)
        .ok_or_else(|| invalid("Invalid cluster API path."))?;
    let uri = format!(
        "/api/{tail}{}",
        request
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    *request.uri_mut() = uri
        .parse::<Uri>()
        .map_err(|_| invalid("Invalid cluster API path."))?;
    // Remove the outer router's captured parameters before the feature router
    // extracts app/project names. Authentication has already run on the outer API.
    request.extensions_mut().clear();
    Ok(features()
        .with_state(state)
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
        .into_response())
}
pub(crate) fn routes(manager: Manager) -> Router<AppState> {
    Router::new()
        .route("/api/clusters", get(list).post(create))
        .route("/api/clusters/test", post(candidate))
        .route("/api/clusters/{id}", post(update).delete(disconnect))
        .route("/api/clusters/{id}/test", post(test))
        .route("/api/clusters/{id}/{*tail}", any(dispatch))
        .with_state(manager)
}
