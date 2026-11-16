#[cfg(test)]
use k8s_openapi::api::networking::v1::Ingress;
pub(crate) mod domains;
use crate::{
    applications::models::{App, Deploy},
    error::{Error, Result, invalid},
    http::request::mutation,
    https, projects,
    state::AppState,
    storage,
    validation::{valid_name, validate},
    workloads::{LABEL, managed},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::{
    Api, ResourceExt,
    api::{ListParams, Patch, PatchParams},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
    task::{JoinHandle, JoinSet},
};

#[derive(Clone, Default)]
pub struct SettingsState {
    demo: Arc<RwLock<BTreeMap<String, Config>>>,
    forwards: Arc<Mutex<BTreeMap<String, Forward>>>,
    domain_gate: Arc<Mutex<()>>,
}
impl SettingsState {
    pub(crate) async fn stop_all_forwards(&self) {
        self.forwards.lock().await.clear();
    }
    pub async fn initialize_demo(&self, app: &App, port: u16) {
        let mut config = defaults(app);
        config.port = port;
        self.demo.write().await.insert(app.name.clone(), config);
    }
}
struct Forward {
    port: u16,
    task: JoinHandle<()>,
}
impl Drop for Forward {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Config {
    project_id: Option<String>,
    workload_kind: String,
    max_replicas: i32,
    image: String,
    replicas: i32,
    port: u16,
    cpu_request: String,
    cpu_limit: String,
    memory_request: String,
    memory_limit: String,
    env: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) domain: Domain,
    #[serde(default)]
    pub(crate) domains: Vec<Domain>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Domain {
    #[serde(default)]
    pub(crate) id: String,
    pub(crate) host: String,
    pub(crate) path: String,
    pub(crate) ingress_class: String,
    pub(crate) tls_secret: String,
    #[serde(default)]
    pub(crate) tls_mode: String,
    #[serde(default)]
    pub(crate) issuer: String,
    #[serde(default)]
    pub(crate) issuer_kind: String,
}
#[derive(Deserialize)]
struct General {
    image: String,
    replicas: i32,
}
#[derive(Deserialize)]
struct Port {
    port: u16,
}
#[derive(Deserialize)]
struct Resources {
    cpu_request: String,
    cpu_limit: String,
    memory_request: String,
    memory_limit: String,
}
#[derive(Deserialize)]
struct Environment {
    env: BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct ForwardRequest {
    local_port: u16,
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/{name}/config", get(config))
        .route("/api/apps/{name}/general", post(general))
        .route("/api/apps/{name}/port", post(port))
        .route("/api/apps/{name}/environment", post(environment))
        .route("/api/apps/{name}/resources", post(resources))
        .merge(domains::routes())
        .route("/api/apps/{name}/domain", post(domain))
        .route("/api/apps/{name}/forward", post(forward))
        .route("/api/apps/{name}/forward/stop", post(stop_forward))
}
fn defaults(app: &App) -> Config {
    Config {
        project_id: app.project_id.clone(),
        workload_kind: app.workload_kind.clone(),
        max_replicas: if app.workload_kind == "StatefulSet" {
            1
        } else {
            20
        },
        image: app.image.clone(),
        replicas: app.replicas,
        port: 80,
        cpu_request: "100m".into(),
        cpu_limit: "1".into(),
        memory_request: "64Mi".into(),
        memory_limit: "512Mi".into(),
        env: BTreeMap::new(),
        domain: Domain::default(),
        domains: Vec::new(),
    }
}
pub(crate) async fn load(s: &AppState, name: &str) -> Result<Config> {
    if s.client.is_none() {
        let apps = s.apps.read().await;
        let a = apps
            .get(name)
            .ok_or_else(|| Error(StatusCode::NOT_FOUND, "App not found".into()))?;
        let mut cfg = s
            .settings
            .demo
            .read()
            .await
            .get(name)
            .cloned()
            .unwrap_or_else(|| defaults(a));
        cfg.project_id = a.project_id.clone();
        cfg.image = a.image.clone();
        cfg.replicas = a.replicas;
        cfg.max_replicas = if a.workload_kind == "StatefulSet"
            || s.storage
                .demo
                .read()
                .await
                .get(name)
                .is_some_and(|v| !v.is_empty())
        {
            1
        } else {
            20
        };
        return Ok(cfg);
    }
    let api = managed(s, name).await?;
    let d = api.get(name).await?;
    let max_replicas = if api.kind == "StatefulSet"
        || storage::has_pvc(&serde_json::to_value(&d).unwrap_or(Value::Null))
    {
        1
    } else {
        20
    };
    let project_id = d.labels().get(projects::PROJECT_LABEL).cloned();
    let spec = d.spec.ok_or_else(|| invalid("Deployment has no spec"))?;
    let c = spec
        .template
        .spec
        .and_then(|p| p.containers.into_iter().find(|c| c.name == "app"))
        .ok_or_else(|| invalid("Managed app container not found"))?;
    let resources = c.resources.as_ref();
    let quantity = |limit: bool, key: &str, fallback: &str| -> String {
        resources
            .and_then(|r| {
                if limit {
                    r.limits.as_ref()
                } else {
                    r.requests.as_ref()
                }
            })
            .and_then(|m| m.get(key))
            .map(|q| q.0.clone())
            .unwrap_or_else(|| fallback.into())
    };
    let mut cfg = Config {
        workload_kind: api.kind,
        max_replicas,
        project_id,
        image: c.image.clone().unwrap_or_default(),
        replicas: spec.replicas.unwrap_or(1),
        port: c
            .ports
            .as_ref()
            .and_then(|p| p.first())
            .map(|p| p.container_port as u16)
            .unwrap_or(80),
        cpu_request: quantity(false, "cpu", "100m"),
        cpu_limit: quantity(true, "cpu", "1"),
        memory_request: quantity(false, "memory", "64Mi"),
        memory_limit: quantity(true, "memory", "512Mi"),
        env: c
            .env
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| e.value.map(|v| (e.name, v)))
            .collect(),
        domain: Domain::default(),
        domains: Vec::new(),
    };
    cfg.domains = domains::load(s, name).await?;
    cfg.domain = cfg.domains.first().cloned().unwrap_or_default();
    Ok(cfg)
}
async fn config(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    let cfg = load(&s, &name).await?;
    let forwards = s.settings.forwards.lock().await;
    let forward = forwards
        .get(&name)
        .filter(|f| !f.task.is_finished())
        .map(|f| json!({"local_port":f.port}));
    Ok(Json(
        json!({"config":cfg,"namespace":s.namespace,"mode":if s.client.is_some(){"live"}else{"demo"},"forward":forward}),
    ))
}
async fn patch_container(s: &AppState, name: &str, patch: Value) -> Result<()> {
    managed(s, name)
        .await?
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Strategic(json!({"spec":{"template":{"spec":{"containers":[patch]}}}})),
        )
        .await?;
    Ok(())
}
async fn general(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(g): Json<General>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _demo_gate = if s.client.is_none() {
        Some(s.storage.gate.lock().await)
    } else {
        None
    };
    storage::demo_limit(&s, &name, g.replicas).await?;
    validate(&Deploy {
        workload_kind: String::new(),
        project_id: None,
        name: name.clone(),
        image: g.image.clone(),
        port: 80,
        replicas: g.replicas,
    })?;
    if s.client.is_some() {
        managed(&s,&name).await?.patch(&name,&PatchParams::default(),&Patch::Strategic(json!({"spec":{"replicas":g.replicas,"template":{"spec":{"containers":[{"name":"app","image":g.image}]}}}}))).await?;
    } else {
        let mut apps = s.apps.write().await;
        let a = apps
            .get_mut(&name)
            .ok_or_else(|| Error(StatusCode::NOT_FOUND, "App not found".into()))?;
        a.image = g.image;
        a.replicas = g.replicas;
        a.ready = g.replicas;
        a.status = if g.replicas == 0 { "Paused" } else { "Healthy" }.into();
    }
    Ok(Json(json!({"ok":true})))
}
async fn own_service(s: &AppState, name: &str) -> Result<(Api<Service>, Service)> {
    let api: Api<Service> = Api::namespaced(s.client.clone().unwrap(), &s.namespace);
    let service = api.get(name).await?;
    if service
        .labels()
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some("kubeweasel")
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Service is not managed by KubeWeasel".into(),
        ));
    }
    Ok((api, service))
}
async fn port(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(p): Json<Port>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    if p.port == 0 {
        return Err(invalid("Port must be 1–65535."));
    }
    let mut cfg = load(&s, &name).await?;
    if s.client.is_some() {
        let (api, _) = own_service(&s, &name).await?;
        // Keep the Service's public port stable so existing Ingress rules still work.
        api.patch(&name,&PatchParams::default(),&Patch::Merge(json!({"spec":{"ports":[{"port":service_port(&s,&name).await?,"targetPort":p.port,"protocol":"TCP"}]}}))).await?;
        if let Err(e) = patch_container(
            &s,
            &name,
            json!({"name":"app","ports":[{"$patch":"replace"},{"containerPort":p.port,"protocol":"TCP"}]}),
        )
        .await
        {
            return Err(Error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "Service updated, but Deployment update failed: {}. Retry saving the container port.",
                    e.1
                ),
            ));
        }
    } else {
        cfg.port = p.port;
        s.settings.demo.write().await.insert(name.clone(), cfg);
    }
    s.settings.forwards.lock().await.remove(&name);
    Ok(Json(json!({"ok":true})))
}
async fn service_port(s: &AppState, name: &str) -> Result<i32> {
    let (_, service) = own_service(s, name).await?;
    service
        .spec
        .and_then(|s| s.ports)
        .and_then(|p| p.first().map(|p| p.port))
        .ok_or_else(|| invalid("Service has no ports"))
}
async fn environment(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(e): Json<Environment>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    if e.env.len() > 100 || e.env.iter().any(|(k, v)| !valid_env(k) || v.len() > 16384) {
        return Err(invalid(
            "Use up to 100 environment variables with valid names and values under 16 KiB.",
        ));
    }
    let mut cfg = load(&s, &name).await?;
    if s.client.is_some() {
        // Preserve valueFrom references (Secrets/ConfigMaps); this editor owns literal values only.
        let api = managed(&s, &name).await?;
        let d = api.get(&name).await?;
        let mut all = d
            .spec
            .and_then(|d| d.template.spec)
            .and_then(|p| p.containers.into_iter().find(|c| c.name == "app"))
            .and_then(|c| c.env)
            .unwrap_or_default();
        let referenced = all
            .iter()
            .filter(|v| v.value_from.is_some())
            .map(|v| v.name.clone())
            .collect::<Vec<_>>();
        if e.env.keys().any(|k| referenced.contains(k)) {
            return Err(invalid(
                "A variable with that name references a Secret or ConfigMap; edit that reference outside this editor.",
            ));
        }
        all.retain(|v| v.value_from.is_some());
        for (k, v) in e.env {
            all.push(k8s_openapi::api::core::v1::EnvVar {
                name: k,
                value: Some(v),
                value_from: None,
            });
        }
        let mut env = vec![json!({"$patch":"replace"})];
        env.extend(all.into_iter().map(|v| json!(v)));
        patch_container(&s, &name, json!({"name":"app","env":env})).await?;
    } else {
        cfg.env = e.env;
        s.settings.demo.write().await.insert(name, cfg);
    }
    Ok(Json(json!({"ok":true})))
}
fn valid_env(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .enumerate()
            .all(|(i, c)| c == b'_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}
fn validate_resources(r: &Resources) -> Result<()> {
    let cpu = |s: &str| -> Option<f64> {
        if let Some(n) = s.strip_suffix('m') {
            n.parse::<f64>().ok().map(|v| v / 1000.)
        } else {
            s.parse().ok()
        }
    };
    let memory = |s: &str| -> Option<f64> {
        for (suffix, factor) in [("Ki", 1024.), ("Mi", 1048576.), ("Gi", 1073741824.)] {
            if let Some(n) = s.strip_suffix(suffix) {
                return n.parse::<f64>().ok().map(|v| v * factor);
            }
        }
        s.parse().ok()
    };
    for (request, limit) in [
        (cpu(&r.cpu_request), cpu(&r.cpu_limit)),
        (memory(&r.memory_request), memory(&r.memory_limit)),
    ] {
        match (request, limit) {
            (Some(a), Some(b)) if a.is_finite() && b.is_finite() && a > 0. && b >= a => {}
            _ => {
                return Err(invalid(
                    "Use positive resource quantities, with requests no higher than limits. CPU: 100m or 1. Memory: 64Mi or 1Gi.",
                ));
            }
        }
    }
    Ok(())
}
async fn resources(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(r): Json<Resources>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate_resources(&r)?;
    let mut cfg = load(&s, &name).await?;
    if s.client.is_some() {
        patch_container(&s,&name,json!({"name":"app","resources":{"requests":{"cpu":r.cpu_request,"memory":r.memory_request},"limits":{"cpu":r.cpu_limit,"memory":r.memory_limit}}})).await?;
    } else {
        cfg.cpu_request = r.cpu_request;
        cfg.cpu_limit = r.cpu_limit;
        cfg.memory_request = r.memory_request;
        cfg.memory_limit = r.memory_limit;
        s.settings.demo.write().await.insert(name, cfg);
    }
    Ok(Json(json!({"ok":true})))
}
fn validate_domain(d: &Domain) -> Result<()> {
    if d.host.is_empty() {
        return Ok(());
    }
    if d.host.len() > 253
        || !d.host.split('.').all(valid_name)
        || !d.path.starts_with('/')
        || d.path.len() > 1024
        || d.path.chars().any(char::is_whitespace)
        || (!d.ingress_class.is_empty() && !valid_name(&d.ingress_class))
        || (!d.tls_secret.is_empty()
            && (!d.tls_secret.split('.').all(valid_name) || d.tls_secret.len() > 253))
    {
        return Err(invalid(
            "Enter a lowercase hostname without scheme or port, an absolute path, and valid class/secret names.",
        ));
    }
    Ok(())
}
fn ingress(name: &str, d: &Domain, port: i32) -> Value {
    let mut spec = json!({"rules":[{"host":d.host,"http":{"paths":[{"path":d.path,"pathType":"Prefix","backend":{"service":{"name":name,"port":{"number":port}}}}]}}]});
    if !d.ingress_class.is_empty() {
        spec["ingressClassName"] = json!(d.ingress_class);
    }
    if !d.tls_secret.is_empty() {
        spec["tls"] = json!([{"hosts":[d.host],"secretName":d.tls_secret}]);
    }
    let mut annotations = https::annotations(d);
    annotations
        .as_object_mut()
        .unwrap()
        .retain(|_, v| !v.is_null());
    json!({"apiVersion":"networking.k8s.io/v1","kind":"Ingress","metadata":{"name":name,"labels":{"app.kubernetes.io/managed-by":"kubeweasel"},"annotations":annotations},"spec":spec})
}
pub(crate) async fn domain(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(mut d): Json<Domain>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _guard = s.settings.domain_gate.lock().await;
    d.id = name.clone();
    let id = domains::save(&s, &name, d, true).await?;
    Ok(Json(json!({"ok":true,"id":id})))
}

async fn forward(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(f): Json<ForwardRequest>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let cfg = load(&s, &name).await?;
    if f.local_port < 1024 {
        return Err(invalid("Choose a local port from 1024 to 65535."));
    }
    let c = s
        .client
        .clone()
        .ok_or_else(|| invalid("Port forwarding needs a live Kubernetes connection."))?;
    let mut forwards = s.settings.forwards.lock().await;
    forwards.retain(|_, f| !f.task.is_finished());
    if forwards.contains_key(&name) {
        return Err(Error(
            StatusCode::CONFLICT,
            "Stop the existing forward before starting another.".into(),
        ));
    }
    if forwards.len() >= 10 {
        return Err(invalid("At most 10 forwards may run at once."));
    }
    let pods: Api<Pod> = Api::namespaced(c, &s.namespace);
    let selector = format!("{LABEL},app.kubernetes.io/name={name}");
    let pod = ready_pod(&pods, &selector).await?;
    // Verify Kubernetes access before reporting that the tunnel has started.
    let probe = pods.portforward(&pod, &[cfg.port]).await?;
    drop(probe);
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, f.local_port))
        .await
        .map_err(|e| invalid(&format!("Cannot bind local port: {e}")))?;
    let task = tokio::spawn(async move {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {accepted=listener.accept()=>{let Ok((mut socket,_))=accepted else{break;};if connections.len()>=32{continue;}let pods=pods.clone();let selector=selector.clone();connections.spawn(async move{if let Ok(pod)=ready_pod(&pods,&selector).await&& let Ok(mut pf)=pods.portforward(&pod,&[cfg.port]).await&& let Some(mut stream)=pf.take_stream(cfg.port){let _=tokio::time::timeout(std::time::Duration::from_secs(300),tokio::io::copy_bidirectional(&mut socket,&mut stream)).await;}});},_=connections.join_next(),if !connections.is_empty()=>{},_=tokio::time::sleep(std::time::Duration::from_secs(3600))=>break}
        }
    });
    forwards.insert(
        name,
        Forward {
            port: f.local_port,
            task,
        },
    );
    Ok(Json(json!({"ok":true,"local_port":f.local_port})))
}
async fn ready_pod(pods: &Api<Pod>, selector: &str) -> Result<String> {
    pods.list(&ListParams::default().labels(selector))
        .await?
        .items
        .into_iter()
        .find(|p| {
            p.metadata.deletion_timestamp.is_none()
                && p.status
                    .as_ref()
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|conditions| {
                        conditions
                            .iter()
                            .any(|c| c.type_ == "Ready" && c.status == "True")
                    })
        })
        .map(|p| p.name_any())
        .ok_or_else(|| invalid("No ready pods available. Wait for the app to become healthy."))
}
async fn stop_forward(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    load(&s, &name).await?;
    s.settings.forwards.lock().await.remove(&name);
    Ok(Json(json!({"ok":true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn domain_rejects_urls_and_invalid_names() {
        let mut d = Domain {
            host: "web.example.com".into(),
            path: "/".into(),
            ..Default::default()
        };
        assert!(validate_domain(&d).is_ok());
        for host in [
            "https://example.com",
            "evil.com/path",
            "a..com",
            "UPPER.com",
        ] {
            d.host = host.into();
            assert!(validate_domain(&d).is_err());
        }
    }
    #[test]
    fn ingress_uses_service_port_and_explicit_tls() {
        let d = Domain {
            host: "web.example.com".into(),
            path: "/app".into(),
            ingress_class: "nginx".into(),
            tls_secret: "web-tls".into(),
            ..Default::default()
        };
        let i = ingress("web", &d, 8080);
        assert_eq!(
            i["spec"]["rules"][0]["http"]["paths"][0]["backend"]["service"]["port"]["number"],
            8080
        );
        assert_eq!(i["spec"]["tls"][0]["secretName"], "web-tls");
        assert!(serde_json::from_value::<Ingress>(i).is_ok());
    }
    #[test]
    fn environment_names() {
        assert!(valid_env("APP_PORT"));
        assert!(!valid_env("1NAME"));
        assert!(!valid_env("A=B"));
    }
    #[test]
    fn resource_bounds() {
        let mut r = Resources {
            cpu_request: "100m".into(),
            cpu_limit: "1".into(),
            memory_request: "64Mi".into(),
            memory_limit: "512Mi".into(),
        };
        assert!(validate_resources(&r).is_ok());
        r.cpu_request = "2".into();
        assert!(validate_resources(&r).is_err());
        r.cpu_request = "NaN".into();
        assert!(validate_resources(&r).is_err());
    }
}
