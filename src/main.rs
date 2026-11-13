mod auth;
mod diagnostics;
mod git;
mod history;
mod https;
mod monitoring;
mod projects;
mod registry;
mod settings;
mod storage;
mod workloads;
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
    projects: projects::Store,
    storage: storage::StorageState,
}
#[derive(Clone, Serialize)]
struct App {
    name: String,
    image: String,
    replicas: i32,
    ready: i32,
    status: String,
    workload_kind: String,
    project_id: Option<String>,
}
#[derive(Deserialize)]
struct Deploy {
    #[serde(default)]
    workload_kind: String,
    #[serde(default)]
    project_id: Option<String>,
    name: String,
    image: String,
    port: u16,
    replicas: i32,
}
#[derive(Deserialize)]
struct Scale {
    replicas: i32,
}
#[derive(Debug)]
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
    if !matches!(d.workload_kind.as_str(), "" | "Deployment" | "StatefulSet")
        || (d.workload_kind == "StatefulSet" && d.replicas > 1)
    {
        return Err(invalid(
            "Choose Deployment or a single-replica StatefulSet.",
        ));
    }
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
        workloads::list(c, &s.namespace, LABEL)
            .await?
            .into_iter()
            .map(|(kind, d)| {
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
                    workload_kind: kind,
                    project_id: d.labels().get(projects::PROJECT_LABEL).cloned(),
                    name: d.name_any(),
                    image,
                    replicas,
                    ready,
                    status: if d
                        .annotations()
                        .get("kubeweasel.io/build-status")
                        .map(String::as_str)
                        == Some("Building")
                    {
                        "Building"
                    } else if replicas == 0
                        && d.annotations()
                            .get("kubeweasel.io/build-status")
                            .map(String::as_str)
                            == Some("Failed")
                    {
                        "Build failed"
                    } else if replicas == 0 {
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
    deploy_with_pull_secret(s, headers, d, None).await
}
async fn deploy_with_pull_secret(
    s: AppState,
    headers: HeaderMap,
    d: Deploy,
    pull_secret: Option<String>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate(&d)?;
    let _project_gate = s.projects.gate.lock().await;
    let _storage_gate = s.storage.gate.lock().await;
    let kind = if d.workload_kind.is_empty() {
        "Deployment".to_string()
    } else {
        d.workload_kind.clone()
    };
    s.projects.require(&s.namespace, d.project_id.as_deref())?;
    let mut labels = BTreeMap::from([(
        "app.kubernetes.io/managed-by".to_string(),
        "kubeweasel".to_string(),
    )]);
    if let Some(id) = &d.project_id {
        labels.insert(projects::PROJECT_LABEL.into(), id.clone());
    }
    if let Some(c) = s.client {
        workloads::name_available(c.clone(), &s.namespace, &d.name).await?;
        let mut deployment:Deployment=serde_json::from_value(json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":d.name,"labels":labels},"spec":{"replicas":d.replicas,"selector":{"matchLabels":{"app.kubernetes.io/name":d.name}},"template":{"metadata":{"labels":{"app.kubernetes.io/name":d.name,"app.kubernetes.io/managed-by":"kubeweasel"}},"spec":{"containers":[{"name":"app","image":d.image,"ports":[{"containerPort":d.port}],"resources":{"requests":{"cpu":"100m","memory":"64Mi"},"limits":{"cpu":"1","memory":"512Mi"}}}]}}}})).map_err(|_|invalid("Invalid deployment"))?;
        registry::set_pull_secret(&mut deployment, pull_secret);
        workloads::create(
            c.clone(),
            &s.namespace,
            &kind,
            serde_json::to_value(&deployment).map_err(|_| invalid("Invalid workload"))?,
        )
        .await?;
        let services: Api<Service> = Api::namespaced(c, &s.namespace);
        let mut service:Service=serde_json::from_value(json!({"apiVersion":"v1","kind":"Service","metadata":{"name":d.name,"labels":labels},"spec":{"selector":{"app.kubernetes.io/name":d.name},"ports":[{"port":d.port,"targetPort":d.port}]}})).map_err(|_|invalid("Invalid service"))?;
        if kind == "StatefulSet"
            && let Some(spec) = service.spec.as_mut()
        {
            spec.cluster_ip = Some("None".into());
        }
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
            workload_kind: kind,
            project_id: d.project_id,
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
async fn managed(s: &AppState, name: &str) -> Result<workloads::Managed> {
    workloads::managed(s, name).await
}
async fn scale(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Scale>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _demo_gate = if s.client.is_none() {
        Some(s.storage.gate.lock().await)
    } else {
        None
    };
    storage::demo_limit(&s, &name, body.replicas).await?;
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
    let address = std::env::var("KUBEWEASEL_BIND").unwrap_or_else(|_| "127.0.0.1:7878".into());
    if std::env::args().any(|arg| arg == "--healthcheck") {
        let bind: std::net::SocketAddr = address.parse()?;
        let ip = if bind.ip().is_unspecified() {
            if bind.is_ipv4() {
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            }
        } else {
            bind.ip()
        };
        let mut request = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?
            .get(format!(
                "http://{}/healthz",
                std::net::SocketAddr::new(ip, bind.port())
            ));
        if let Ok(origin) = std::env::var("KUBEWEASEL_PUBLIC_ORIGIN") {
            let origin = reqwest::Url::parse(&origin)?.origin().ascii_serialization();
            request = request.header("host", origin.trim_start_matches("https://"));
        }
        request.send().await?.error_for_status()?;
        return Ok(());
    }
    let auth = auth::AuthState::open(
        &std::env::var("KUBEWEASEL_AUTH_DB").unwrap_or_else(|_| "kubeweasel.db".into()),
        &address,
        std::env::var("KUBEWEASEL_PUBLIC_ORIGIN").ok(),
    )?;
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
                    workload_kind: "Deployment".into(),
                    project_id: None,
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
        storage: storage::StorageState::default(),
        projects: if demo {
            projects::Store::open(":memory:")?
        } else {
            projects::Store::open(
                &std::env::var("KUBEWEASEL_AUTH_DB").unwrap_or_else(|_| "kubeweasel.db".into()),
            )?
        },
    };
    git::worker(s.clone());
    history::worker(s.clone());
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
        .merge(history::routes())
        .route(
            "/history.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/history.js"),
                )
            }),
        )
        .merge(registry::routes())
        .merge(git::routes())
        .route(
            "/git.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/git.js"),
                )
            }),
        )
        .merge(settings::routes())
        .merge(storage::routes())
        .route(
            "/storage.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/storage.js"),
                )
            }),
        )
        .merge(projects::routes())
        .route(
            "/projects.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/projects.js"),
                )
            }),
        )
        .merge(diagnostics::routes())
        .merge(https::routes())
        .route(
            "/https.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/https.js"),
                )
            }),
        )
        .merge(monitoring::routes())
        .route(
            "/monitoring.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/monitoring.js"),
                )
            }),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/overview", get(overview))
        .route("/api/apps", post(deploy))
        .route("/api/apps/{name}/scale", post(scale))
        .route("/api/apps/{name}/restart", post(restart))
        .route("/api/apps/{name}/logs", get(logs))
        .merge(auth::routes(auth.clone()))
        .route(
            "/auth.js",
            get(|| async {
                (
                    [("content-type", "text/javascript")],
                    include_str!("../dashboard/auth.js"),
                )
            }),
        )
        .layer(axum::middleware::from_fn_with_state(auth, auth::guard))
        .with_state(s);
    let address = std::env::var("KUBEWEASEL_BIND").unwrap_or_else(|_| "127.0.0.1:7878".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!(
        "KubeWeasel → http://{address} ({})",
        if demo { "demo" } else { "live" }
    );
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
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
            workload_kind: String::new(),
            project_id: None,
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
