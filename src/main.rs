mod diagnostics;
mod settings;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use k8s_openapi::api::{
    apps::v1::Deployment,
    core::v1::{Pod, Service},
};
use kube::{
    Api, Client, ResourceExt,
    api::{ListParams, LogParams, Patch, PatchParams, PostParams},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;

const LABEL: &str = "app.kubernetes.io/managed-by=kubeweasel";
#[derive(Clone)]
struct AppState {
    client: Option<Client>,
    namespace: String,
    apps: Arc<RwLock<BTreeMap<String, App>>>,
    settings: settings::SettingsState,
}
#[derive(Clone, Serialize)]
struct App {
    name: String,
    image: String,
    replicas: i32,
    ready: i32,
    status: String,
}
#[derive(Deserialize)]
struct Deploy {
    name: String,
    image: String,
    port: u16,
    replicas: i32,
}
#[derive(Deserialize)]
struct Scale {
    replicas: i32,
}
struct Error(StatusCode, String);
impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<kube::Error> for Error {
    fn from(e: kube::Error) -> Self {
        Self(StatusCode::BAD_GATEWAY, e.to_string())
    }
}
type Result<T> = std::result::Result<T, Error>;
fn invalid(message: &str) -> Error {
    Error(StatusCode::BAD_REQUEST, message.into())
}
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
}
fn validate(d: &Deploy) -> Result<()> {
    if !valid_name(&d.name) {
        return Err(invalid("Use a lowercase DNS name, up to 63 characters."));
    }
    if d.image.is_empty() || d.image.len() > 512 || d.image.chars().any(char::is_whitespace) {
        return Err(invalid("Enter a valid container image reference."));
    }
    if d.port == 0 || !(0..=20).contains(&d.replicas) {
        return Err(invalid("Port must be 1–65535; replicas must be 0–20."));
    }
    Ok(())
}
fn mutation(headers: &HeaderMap) -> Result<()> {
    if headers
        .get("x-kubeweasel-request")
        .and_then(|h| h.to_str().ok())
        != Some("dashboard")
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Missing dashboard request header.".into(),
        ));
    }
    Ok(())
}
async fn overview(State(s): State<AppState>) -> Result<Json<Value>> {
    let apps = if let Some(c) = s.client.clone() {
        let api: Api<Deployment> = Api::namespaced(c, &s.namespace);
        api.list(&ListParams::default().labels(LABEL))
            .await?
            .items
            .into_iter()
            .map(|d| {
                let spec = d.spec.as_ref();
                let ready = d
                    .status
                    .as_ref()
                    .and_then(|s| s.ready_replicas)
                    .unwrap_or(0);
                let replicas = spec.and_then(|s| s.replicas).unwrap_or(1);
                let image = spec
                    .and_then(|s| s.template.spec.as_ref())
                    .and_then(|s| s.containers.first())
                    .and_then(|c| c.image.clone())
                    .unwrap_or_default();
                App {
                    name: d.name_any(),
                    image,
                    replicas,
                    ready,
                    status: if replicas == 0 {
                        "Paused"
                    } else if ready >= replicas {
                        "Healthy"
                    } else {
                        "Deploying"
                    }
                    .into(),
                }
            })
            .collect::<Vec<_>>()
    } else {
        s.apps.read().await.values().cloned().collect()
    };
    Ok(Json(
        json!({"mode":if s.client.is_some(){"live"}else{"demo"},"namespace":s.namespace,"apps":apps}),
    ))
}
async fn deploy(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(d): Json<Deploy>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate(&d)?;
    if let Some(c) = s.client {
        let api: Api<Deployment> = Api::namespaced(c.clone(), &s.namespace);
        let deployment:Deployment=serde_json::from_value(json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":d.name,"labels":{"app.kubernetes.io/managed-by":"kubeweasel"}},"spec":{"replicas":d.replicas,"selector":{"matchLabels":{"app.kubernetes.io/name":d.name}},"template":{"metadata":{"labels":{"app.kubernetes.io/name":d.name,"app.kubernetes.io/managed-by":"kubeweasel"}},"spec":{"containers":[{"name":"app","image":d.image,"ports":[{"containerPort":d.port}],"resources":{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"cpu":"1","memory":"512Mi"}}}]}}}})).map_err(|_|invalid("Invalid deployment"))?;
        api.create(&PostParams::default(), &deployment).await?;
        let services: Api<Service> = Api::namespaced(c, &s.namespace);
        let service:Service=serde_json::from_value(json!({"apiVersion":"v1","kind":"Service","metadata":{"name":d.name,"labels":{"app.kubernetes.io/managed-by":"kubeweasel"}},"spec":{"selector":{"app.kubernetes.io/name":d.name},"ports":[{"port":d.port,"targetPort":d.port}]}})).map_err(|_|invalid("Invalid service"))?;
        if let Err(e) = services.create(&PostParams::default(), &service).await {
            return Err(Error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "Deployment created, but Service creation failed: {e}. Inspect the resources before retrying."
                ),
            ));
        }
    } else {
        let mut apps = s.apps.write().await;
        if apps.contains_key(&d.name) {
            return Err(Error(
                StatusCode::CONFLICT,
                "An app with this name already exists.".into(),
            ));
        }
        let app = App {
            name: d.name,
            image: d.image,
            replicas: d.replicas,
            ready: d.replicas,
            status: if d.replicas == 0 { "Paused" } else { "Healthy" }.into(),
        };
        s.settings.initialize_demo(&app, d.port).await;
        apps.insert(app.name.clone(), app);
    }
    Ok(Json(json!({"ok":true})))
}
async fn managed(s: &AppState, name: &str) -> Result<Api<Deployment>> {
    let api = Api::namespaced(
        s.client.clone().ok_or_else(|| invalid("No cluster"))?,
        &s.namespace,
    );
    let d: Deployment = api.get(name).await?;
    if d.labels()
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some("kubeweasel")
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "This deployment is not managed by KubeWeasel.".into(),
        ));
    }
    Ok(api)
}
async fn scale(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Scale>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    if !(0..=20).contains(&body.replicas) {
        return Err(invalid("Replicas must be 0–20."));
    }
    if s.client.is_some() {
        managed(&s, &name)
            .await?
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({"spec":{"replicas":body.replicas}})),
            )
            .await?;
    } else {
        let mut apps = s.apps.write().await;
        let app = apps
            .get_mut(&name)
            .ok_or_else(|| Error(StatusCode::NOT_FOUND, "App not found".into()))?;
        app.replicas = body.replicas;
        app.ready = body.replicas;
        app.status = if body.replicas == 0 {
            "Paused"
        } else {
            "Healthy"
        }
        .into();
    }
    Ok(Json(json!({"ok":true})))
}
async fn restart(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    if s.client.is_some() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string();
        managed(&s,&name).await?.patch(&name,&PatchParams::default(),&Patch::Merge(json!({"spec":{"template":{"metadata":{"annotations":{"kubeweasel.io/restarted-at":stamp}}}}}))).await?;
    } else if !s.apps.read().await.contains_key(&name) {
        return Err(Error(StatusCode::NOT_FOUND, "App not found".into()));
    }
    Ok(Json(json!({"ok":true})))
}
async fn logs(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    if let Some(c) = s.client.clone() {
        managed(&s, &name).await?;
        let pods: Api<Pod> = Api::namespaced(c, &s.namespace);
        let list = pods
            .list(&ListParams::default().labels(&format!("{LABEL},app.kubernetes.io/name={name}")))
            .await?;
        let mut output = String::new();
        for p in list.items.into_iter().take(5) {
            let n = p.name_any();
            output.push_str(&format!("--- {n} ---\n"));
            match pods
                .logs(
                    &n,
                    &LogParams {
                        tail_lines: Some(100),
                        container: Some("app".into()),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(log) => output.push_str(&log),
                Err(e) => output.push_str(&e.to_string()),
            };
            output.push('\n');
        }
        Ok(Json(
            json!({"logs":if output.is_empty(){"No pods available yet.".into()}else{output}}),
        ))
    } else {
        if !s.apps.read().await.contains_key(&name) {
            return Err(Error(StatusCode::NOT_FOUND, "App not found".into()));
        }
        Ok(Json(
            json!({"logs":format!("[demo] {name}: container started\n[demo] Application listening for requests\n[demo] These are sample logs, not cluster output.")}),
        ))
    }
}
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let demo = std::env::args().any(|a| a == "--demo");
    let namespace = std::env::var("KUBEWEASEL_NAMESPACE").unwrap_or_else(|_| "default".into());
    let client = if demo {
        None
    } else {
        Some(Client::try_default().await?)
    };
    let mut apps = BTreeMap::new();
    if demo {
        for (name, image, replicas) in [
            ("web-frontend", "nginx:stable", 2),
            ("payments-api", "ghcr.io/example/payments:1.4.2", 3),
            ("background-worker", "ghcr.io/example/worker:0.8.0", 0),
        ] {
            apps.insert(
                name.into(),
                App {
                    name: name.into(),
                    image: image.into(),
                    replicas,
                    ready: replicas,
                    status: if replicas == 0 { "Paused" } else { "Healthy" }.into(),
                },
            );
        }
    }
    let s = AppState {
        client,
        namespace,
        apps: Arc::new(RwLock::new(apps)),
        settings: settings::SettingsState::default(),
    };
    let router = Router::new()
        .route(
            "/",
            get(|| async { Html(include_str!("../dashboard/index.html")) }),
        )
        .route(
            "/dashboard.css",
            get(|| async {
                (
                    [("content-type", "text/css")],
                    include_str!("../dashboard/dashboard.css"),
                )
            }),
        )
        .route(
            "/dashboard.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/dashboard.js"),
                )
            }),
        )
        .route(
            "/settings.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/settings.js"),
                )
            }),
        )
        .merge(settings::routes())
        .merge(diagnostics::routes())
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/overview", get(overview))
        .route("/api/apps", post(deploy))
        .route("/api/apps/{name}/scale", post(scale))
        .route("/api/apps/{name}/restart", post(restart))
        .route("/api/apps/{name}/logs", get(logs))
        .with_state(s);
    let address = std::env::var("KUBEWEASEL_BIND").unwrap_or_else(|_| "127.0.0.1:7878".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!(
        "KubeWeasel → http://{address} ({})",
        if demo { "demo" } else { "live" }
    );
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn https_client_initializes_crypto_provider() {
        // Construct the real TLS stack without contacting a cluster. Demo mode
        // skips this path and cannot detect a missing crypto provider.
        let config = kube::Config::new("https://127.0.0.1:6443".parse().unwrap());
        assert!(Client::try_from(config).is_ok());
    }
    #[test]
    fn rejects_invalid_resource_names() {
        for n in ["", "Hello", "-app", "app-", "a/b", "app.name"] {
            assert!(!valid_name(n));
        }
        assert!(valid_name("payments-api-2"));
    }
    #[test]
    fn bounds_deployments() {
        let mut d = Deploy {
            name: "web".into(),
            image: "nginx:stable".into(),
            port: 80,
            replicas: 2,
        };
        assert!(validate(&d).is_ok());
        d.replicas = 21;
        assert!(validate(&d).is_err());
        d.replicas = 1;
        d.port = 0;
        assert!(validate(&d).is_err());
    }
    #[test]
    fn requires_custom_header() {
        assert!(mutation(&HeaderMap::new()).is_err());
    }
}
