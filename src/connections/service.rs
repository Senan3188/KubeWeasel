use super::models::{
    Attachment, Description, Input, KIND, META, SELECTOR, View, valid_env, validate,
};
use super::usage;
use crate::{
    error::{Error, Result, invalid},
    projects::PROJECT_LABEL,
    state::AppState,
    validation::valid_name,
    workloads,
};
use k8s_openapi::{
    ByteString,
    api::core::v1::{EnvVar, Secret},
};
use kube::{
    Api, ResourceExt,
    api::{
        ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
        PostParams, Preconditions,
    },
};
use rand_core::{OsRng, RngCore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::RwLock;

type DemoBindings = BTreeMap<String, (u64, Vec<EnvVar>)>;
type DemoRevisions = BTreeMap<String, BTreeMap<String, String>>;
#[derive(Clone, Default)]
pub(crate) struct State {
    secrets: Arc<RwLock<BTreeMap<String, Secret>>>,
    bindings: Arc<RwLock<DemoBindings>>,
    applied: Arc<RwLock<DemoRevisions>>,
}
fn store_error() -> Error {
    Error::bad_gateway(
        "Connection storage unavailable. Check namespace Secret permissions; credential details are withheld.",
    )
}
fn description(secret: &Secret) -> Result<Description> {
    if secret
        .labels()
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some("kubeweasel")
        || secret.labels().get(KIND).map(String::as_str) != Some("true")
        || secret.type_.as_deref() != Some("Opaque")
    {
        return Err(invalid("This Secret is not a KubeWeasel connection."));
    }
    let d: Description = serde_json::from_str(
        secret
            .annotations()
            .get(META)
            .ok_or_else(|| invalid("Missing connection metadata."))?,
    )
    .map_err(|_| invalid("Invalid connection metadata."))?;
    if secret.labels().get(PROJECT_LABEL) != Some(&d.project_id) {
        return Err(invalid("Connection project metadata does not match."));
    }
    Ok(d)
}
fn check_id(id: &str) -> Result<()> {
    if !id.starts_with("kw-conn-") || !valid_name(id) {
        return Err(invalid("Invalid connection ID."));
    }
    Ok(())
}
async fn read(s: &AppState, id: &str) -> Result<Secret> {
    check_id(id)?;
    let secret = if let Some(client) = &s.client {
        Api::<Secret>::namespaced(client.clone(), &s.namespace)
            .get_opt(id)
            .await
            .map_err(|_| store_error())?
    } else {
        s.settings.connections.secrets.read().await.get(id).cloned()
    };
    let secret = secret.ok_or_else(|| Error::not_found("Connection not found."))?;
    description(&secret)?;
    Ok(secret)
}
async fn all(s: &AppState) -> Result<Vec<Secret>> {
    if let Some(client) = &s.client {
        Ok(Api::<Secret>::namespaced(client.clone(), &s.namespace)
            .list(&ListParams::default().labels(SELECTOR))
            .await
            .map_err(|_| store_error())?
            .items)
    } else {
        Ok(s.settings
            .connections
            .secrets
            .read()
            .await
            .values()
            .cloned()
            .collect())
    }
}
pub(crate) async fn has_connections(s: &AppState, project: &str) -> Result<bool> {
    Ok(all(s)
        .await?
        .iter()
        .any(|v| v.labels().get(PROJECT_LABEL).map(String::as_str) == Some(project)))
}
pub(crate) async fn app_project(s: &AppState, app: &str) -> Result<Option<String>> {
    if !valid_name(app) {
        return Err(invalid("Invalid application name."));
    }
    if s.client.is_some() {
        Ok(workloads::managed(s, app)
            .await?
            .get(app)
            .await?
            .labels()
            .get(PROJECT_LABEL)
            .cloned())
    } else {
        Ok(s.apps
            .read()
            .await
            .get(app)
            .ok_or_else(|| Error::not_found("Application not found."))?
            .project_id
            .clone())
    }
}
async fn app_env(s: &AppState, app: &str) -> Result<(String, Vec<EnvVar>)> {
    if s.client.is_some() {
        let d = workloads::managed(s, app).await?.get(app).await?;
        let version = d
            .metadata
            .resource_version
            .clone()
            .ok_or_else(|| invalid("Application has no version."))?;
        let env = d
            .spec
            .and_then(|v| v.template.spec)
            .and_then(|p| p.containers.into_iter().find(|c| c.name == "app"))
            .ok_or_else(|| invalid("Application container missing."))?
            .env
            .unwrap_or_default();
        Ok((version, env))
    } else {
        app_project(s, app).await?;
        let entry = s
            .settings
            .connections
            .bindings
            .read()
            .await
            .get(app)
            .cloned()
            .unwrap_or_default();
        let mut env = entry.1;
        env.extend(
            crate::settings::load(s, app)
                .await?
                .env
                .into_iter()
                .map(|(name, value)| EnvVar {
                    name,
                    value: Some(value),
                    value_from: None,
                }),
        );
        let version = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(entry.0, &env))
                    .map_err(|_| invalid("Invalid environment."))?
            )
        );
        Ok((version, env))
    }
}
fn refers(env: &EnvVar, id: &str) -> Option<String> {
    env.value_from
        .as_ref()?
        .secret_key_ref
        .as_ref()
        .filter(|r| r.name == id)
        .map(|r| r.key.clone())
}
/// Inspect pod templates as well as current Pods, including init containers and volumes.
fn references(value: &Value, id: &str) -> bool {
    match value {
        Value::Array(a) => a.iter().any(|v| references(v, id)),
        Value::Object(o) => o.iter().any(|(k, v)| {
            (matches!(k.as_str(), "secretKeyRef" | "secretRef" | "secret")
                && (v.get("name").and_then(Value::as_str) == Some(id)
                    || v.get("secretName").and_then(Value::as_str) == Some(id)))
                || references(v, id)
        }),
        _ => false,
    }
}
// The owning Deployment and remaining Pods are checked separately. Do not let
// inactive Deployment history make an otherwise unused connection undeletable.
fn historical_revision(kind: &str, object: &DynamicObject) -> bool {
    kind == "ReplicaSet"
        && object
            .data
            .pointer("/spec/replicas")
            .and_then(Value::as_i64)
            == Some(0)
        && object
            .metadata
            .owner_references
            .as_ref()
            .is_some_and(|owners| {
                owners
                    .iter()
                    .any(|o| o.kind == "Deployment" && o.controller == Some(true))
            })
}
async fn usages(s: &AppState, id: &str) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    if let Some(client) = &s.client {
        for (group, version, kind) in [
            ("apps", "v1", "Deployment"),
            ("apps", "v1", "StatefulSet"),
            ("apps", "v1", "DaemonSet"),
            ("apps", "v1", "ReplicaSet"),
            ("batch", "v1", "Job"),
            ("batch", "v1", "CronJob"),
            ("", "v1", "Pod"),
        ] {
            let api: Api<DynamicObject> = Api::namespaced_with(
                client.clone(),
                &s.namespace,
                &ApiResource::from_gvk(&GroupVersionKind::gvk(group, version, kind)),
            );
            let objects = api.list(&ListParams::default()).await.map_err(|_| Error::bad_gateway("Cannot verify connection usage. Check workload-list permissions; no connection was deleted."))?;
            for object in objects.items {
                if !historical_revision(kind, &object) && references(&object.data, id) {
                    names.insert(format!("{kind}/{}", object.name_any()));
                }
            }
        }
    } else {
        for (app, (_, env)) in s.settings.connections.bindings.read().await.iter() {
            if env.iter().any(|e| refers(e, id).is_some()) {
                names.insert(format!("Deployment/{app}"));
            }
        }
    }
    Ok(names.into_iter().collect())
}
fn view(secret: &Secret, env: &[EnvVar], workload_version: String) -> Result<View> {
    let id = secret.name_any();
    Ok(View {
        mappings: env
            .iter()
            .filter_map(|e| refers(e, &id).map(|k| (e.name.clone(), k)))
            .collect(),
        id,
        description: description(secret)?,
        keys: secret
            .data
            .as_ref()
            .map(|d| d.keys().cloned().collect())
            .unwrap_or_default(),
        version: secret.metadata.resource_version.clone().unwrap_or_default(),
        workload_version,
        consumers: Vec::new(),
        usage_error: Some("Refresh connections to retrieve application usage.".into()),
    })
}
pub(super) async fn catalog(s: &AppState, project: &str, app: Option<&str>) -> Result<Vec<View>> {
    s.projects.require(&s.namespace, Some(project))?;
    let (version, env) = if let Some(app) = app {
        app_env(s, app).await?
    } else {
        Default::default()
    };
    // Fetch workload status once for the entire catalog, without reading Pod or Secret values.
    let workloads = if let Some(client) = &s.client {
        Some(workloads::list(client.clone(), &s.namespace, workloads::LABEL).await)
    } else {
        None
    };
    let mut result = Vec::new();
    for secret in all(s).await? {
        if secret.labels().get(PROJECT_LABEL).map(String::as_str) == Some(project) {
            let mut entry = view(&secret, &env, version.clone())?;
            entry.usage_error = None;
            let id = secret.name_any();
            let revision = usage::revision(&secret);
            match &workloads {
                Some(Ok(apps)) => {
                    for (kind, app) in apps {
                        if app.labels().get(PROJECT_LABEL).map(String::as_str) == Some(project)
                            && references(&serde_json::to_value(&app.spec).unwrap_or_default(), &id) {
                            entry.consumers.push(usage::consumer(kind, app, &id, &revision));
                        }
                    }
                }
                Some(Err(_)) => entry.usage_error=Some("Application usage is unavailable. Check workload-list permissions; restart status is unknown.".into()),
                None => {
                    let bindings = s.settings.connections.bindings.read().await;
                    let applied = s.settings.connections.applied.read().await;
                    let apps = s.apps.read().await;
                    for (name, (_, env)) in bindings.iter() {
                        if let Some(app)=apps.get(name)
                            && app.project_id.as_deref()==Some(project)
                            && env.iter().any(|e|refers(e,&id).is_some()) {
                            entry.consumers.push(usage::demo_consumer(name,&app.workload_kind,app.replicas==0,&id,&revision,applied.get(name).unwrap_or(&BTreeMap::new())));
                        }
                    }
                }
            }
            entry.consumers.sort_by(|a, b| a.name.cmp(&b.name));
            result.push(entry);
        }
    }
    result.sort_by(|a, b| {
        a.description
            .name
            .to_lowercase()
            .cmp(&b.description.name.to_lowercase())
    });
    Ok(result)
}
pub(crate) async fn has_bindings(s: &AppState, app: &str) -> Result<bool> {
    let (_, env) = app_env(s, app).await?;
    Ok(env.iter().any(|e| {
        e.value_from
            .as_ref()
            .and_then(|v| v.secret_key_ref.as_ref())
            .is_some_and(|r| r.name.starts_with("kw-conn-"))
    }))
}
pub(crate) async fn validate_literals(
    s: &AppState,
    app: &str,
    names: &BTreeMap<String, String>,
) -> Result<()> {
    let (_, env) = app_env(s, app).await?;
    if env.iter().filter(|e| e.value_from.is_some()).count() + names.len() > 100 {
        return Err(invalid(
            "An application supports at most 100 environment variables, including references.",
        ));
    }
    if env
        .iter()
        .any(|e| e.value_from.is_some() && names.contains_key(&e.name))
    {
        return Err(invalid(
            "A variable with this name already references a connection or external configuration. Remove that mapping first.",
        ));
    }
    Ok(())
}

fn expect_version(secret: &Secret, version: &str) -> Result<()> {
    if version.is_empty() || secret.metadata.resource_version.as_deref() != Some(version) {
        return Err(Error::conflict("Connection changed. Refresh and retry."));
    }
    Ok(())
}
fn require_project(secret: &Secret, project: &str) -> Result<()> {
    if description(secret)?.project_id != project {
        return Err(Error::not_found("Connection not found in this project."));
    }
    Ok(())
}
pub(super) async fn save(
    s: &AppState,
    project: &str,
    id: Option<&str>,
    mut input: Input,
) -> Result<View> {
    validate(&input)?;
    if input.description.project_id != project {
        return Err(invalid("Connection project does not match."));
    }
    let _gate = s.projects.gate.lock().await;
    s.projects.require(&s.namespace, Some(project))?;
    let existing = if let Some(id) = id {
        Some(read(s, id).await?)
    } else {
        None
    };
    if let Some(secret) = &existing {
        require_project(secret, project)?;
        expect_version(secret, input.version.as_deref().unwrap_or_default())?;
    }
    let catalog = all(s).await?;
    if catalog
        .iter()
        .filter(|v| Some(v.name_any().as_str()) != id)
        .filter_map(|v| description(v).ok())
        .any(|d| {
            d.project_id == project
                && d.name
                    .trim()
                    .eq_ignore_ascii_case(input.description.name.trim())
        })
    {
        return Err(invalid(
            "A connection with this name already exists in this project.",
        ));
    }
    if existing.is_none()
        && catalog
            .iter()
            .filter(|v| v.labels().get(PROJECT_LABEL).map(String::as_str) == Some(project))
            .count()
            >= 50
    {
        return Err(invalid("A project supports up to 50 connections."));
    }
    let mut secret = existing.clone().unwrap_or_else(|| {
        let mut b = [0u8; 12];
        OsRng.fill_bytes(&mut b);
        let id = format!(
            "kw-conn-{}",
            b.iter().map(|v| format!("{v:02x}")).collect::<String>()
        );
        Secret {
            metadata: kube::core::ObjectMeta {
                name: Some(id),
                labels: Some(BTreeMap::from([
                    ("app.kubernetes.io/managed-by".into(), "kubeweasel".into()),
                    (KIND.into(), "true".into()),
                    (PROJECT_LABEL.into(), project.into()),
                ])),
                ..Default::default()
            },
            type_: Some("Opaque".into()),
            ..Default::default()
        }
    });
    if !input.remove_keys.is_empty() && !usages(s, &secret.name_any()).await?.is_empty() {
        return Err(invalid(
            "Detach all consumers before removing connection keys.",
        ));
    }
    let data = secret.data.get_or_insert_default();
    for key in input.remove_keys {
        data.remove(&key);
    }
    for (key, value) in input.values {
        data.insert(key, ByteString(value.into_bytes()));
    }
    if data.is_empty()
        || data.len() > 100
        || data.values().map(|v| v.0.len()).sum::<usize>() > 65536
    {
        return Err(invalid(
            "A connection needs 1–100 keys with at most 64 KiB total.",
        ));
    }
    if existing.as_ref().is_none_or(|old| {
        old.data != secret.data || !old.annotations().contains_key(usage::REVISION)
    }) {
        secret
            .metadata
            .annotations
            .get_or_insert_default()
            .insert(usage::REVISION.into(), usage::new_revision());
    }
    input.description.name = input.description.name.trim().into();
    secret.metadata.annotations.get_or_insert_default().insert(
        META.into(),
        serde_json::to_string(&input.description)
            .map_err(|_| invalid("Invalid connection metadata."))?,
    );
    secret = if let Some(client) = &s.client {
        let api = Api::<Secret>::namespaced(client.clone(), &s.namespace);
        if existing.is_some() {
            api.replace(&secret.name_any(), &PostParams::default(), &secret)
                .await
        } else {
            api.create(&PostParams::default(), &secret).await
        }
        .map_err(|_| store_error())?
    } else {
        secret.metadata.resource_version = Some(
            (existing
                .as_ref()
                .and_then(|v| v.metadata.resource_version.as_deref())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                + 1)
            .to_string(),
        );
        s.settings
            .connections
            .secrets
            .write()
            .await
            .insert(secret.name_any(), secret.clone());
        secret
    };
    view(&secret, &[], String::new())
}
pub(super) async fn remove(s: &AppState, project: &str, id: &str, version: &str) -> Result<()> {
    let _gate = s.projects.gate.lock().await;
    s.projects.require(&s.namespace, Some(project))?;
    let secret = read(s, id).await?;
    require_project(&secret, project)?;
    expect_version(&secret, version)?;
    if !usages(s, id).await?.is_empty() {
        return Err(Error::conflict(
            "Connection is still referenced by workloads or pods. Detach it and wait for old pods/controllers to finish before deleting.",
        ));
    }
    if let Some(client) = &s.client {
        Api::<Secret>::namespaced(client.clone(), &s.namespace)
            .delete(
                id,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: secret.metadata.uid,
                        resource_version: secret.metadata.resource_version,
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| store_error())?;
    } else {
        s.settings.connections.secrets.write().await.remove(id);
    }
    Ok(())
}
fn mapped_env(
    mut env: Vec<EnvVar>,
    id: &str,
    keys: &BTreeSet<String>,
    input: &Attachment,
    detach: bool,
) -> Result<Vec<EnvVar>> {
    if input.mappings.len() > 100
        || (!detach && input.mappings.is_empty())
        || input
            .mappings
            .iter()
            .any(|(name, key)| !valid_env(name) || !keys.contains(key))
    {
        return Err(invalid(
            "Choose 1–100 valid variable names and existing connection keys.",
        ));
    }
    env.retain(|e| refers(e, id).is_none());
    if !detach {
        if input
            .mappings
            .keys()
            .any(|name| env.iter().any(|e| &e.name == name))
        {
            return Err(invalid(
                "An environment variable with this name already exists. Choose another name or remove it first.",
            ));
        }
        for (name, key) in &input.mappings {
            env.push(
                serde_json::from_value(
                    json!({"name":name,"valueFrom":{"secretKeyRef":{"name":id,"key":key}}}),
                )
                .map_err(|_| invalid("Invalid connection mapping."))?,
            );
        }
    }
    if env.len() > 100 {
        return Err(invalid(
            "An application supports at most 100 environment variables.",
        ));
    }
    Ok(env)
}
pub(super) async fn attach(
    s: &AppState,
    app: &str,
    id: &str,
    input: Attachment,
    detach: bool,
) -> Result<()> {
    let _gate = s.projects.gate.lock().await;
    let project = app_project(s, app)
        .await?
        .ok_or_else(|| invalid("Move this application into a project first."))?;
    s.projects.require(&s.namespace, Some(&project))?;
    let secret = read(s, id).await?;
    require_project(&secret, &project)?;
    expect_version(&secret, &input.version)?;
    let (version, env) = app_env(s, app).await?;
    if version != input.workload_version {
        return Err(Error::conflict("Application changed. Refresh and retry."));
    }
    let revision = usage::revision(&secret);
    let env = mapped_env(
        env,
        id,
        &secret.data.unwrap_or_default().into_keys().collect(),
        &input,
        detach,
    )?;
    if s.client.is_some() {
        let mut values = vec![json!({"$patch":"replace"})];
        values.extend(env.iter().map(|e| json!(e)));
        workloads::managed(s,app).await?.patch(app,&PatchParams::default(),&Patch::Strategic(json!({"metadata":{"resourceVersion":version},"spec":{"template":{"metadata":{"annotations":{(usage::marker(id)):if detach {Value::Null}else{json!(revision)}}},"spec":{"containers":[{"name":"app","env":values}]}}}}))).await?;
    } else {
        let mut bindings = s.settings.connections.bindings.write().await;
        let generation = bindings.get(app).map(|v| v.0).unwrap_or(0) + 1;
        bindings.insert(
            app.into(),
            (
                generation,
                env.into_iter().filter(|e| e.value_from.is_some()).collect(),
            ),
        );
    }
    if s.client.is_none() {
        let mut applied = s.settings.connections.applied.write().await;
        let markers = applied.entry(app.into()).or_default();
        if detach {
            markers.remove(id);
        } else {
            markers.insert(id.into(), revision);
        }
    }
    Ok(())
}
/// Ordinary application restarts also acknowledge the connection revisions being loaded.
pub(crate) async fn restart_application(s: &AppState, app: &str) -> Result<()> {
    let _gate = s.projects.gate.lock().await;
    let project = app_project(s, app).await?;
    let (version, env) = app_env(s, app).await?;
    let ids = env
        .iter()
        .filter_map(|e| {
            e.value_from
                .as_ref()?
                .secret_key_ref
                .as_ref()
                .map(|r| r.name.clone())
        })
        .filter(|id| id.starts_with("kw-conn-"))
        .collect::<BTreeSet<_>>();
    let mut markers = BTreeMap::new();
    let mut revisions = BTreeMap::new();
    for id in ids {
        let secret = read(s, &id).await?;
        require_project(&secret, project.as_deref().unwrap_or_default())?;
        let revision = usage::revision(&secret);
        markers.insert(usage::marker(&id), revision.clone());
        revisions.insert(id, revision);
    }
    if s.client.is_some() {
        markers.insert(
            "kubeweasel.io/restarted-at".into(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .to_string(),
        );
        workloads::managed(s,app).await?.patch(app,&PatchParams::default(),&Patch::Merge(json!({"metadata":{"resourceVersion":version},"spec":{"template":{"metadata":{"annotations":markers}}}}))).await?;
    } else {
        s.settings
            .connections
            .applied
            .write()
            .await
            .entry(app.into())
            .or_default()
            .extend(revisions);
    }
    Ok(())
}
pub(super) async fn restart(s: &AppState, project: &str, id: &str, version: &str) -> Result<Value> {
    let _gate = s.projects.gate.lock().await;
    s.projects.require(&s.namespace, Some(project))?;
    let secret = read(s, id).await?;
    require_project(&secret, project)?;
    expect_version(&secret, version)?;
    // A pod restart reloads every referenced connection, not only the selected one.
    let connections = all(s)
        .await?
        .into_iter()
        .filter(|c| c.labels().get(PROJECT_LABEL).map(String::as_str) == Some(project))
        .collect::<Vec<_>>();
    let mut restarted = Vec::new();
    let mut failed = Vec::new();
    if let Some(client) = &s.client {
        'apps: for (_, app) in
            workloads::list(client.clone(), &s.namespace, workloads::LABEL).await?
        {
            if references(&serde_json::to_value(&app.spec).unwrap_or_default(), id) {
                let name = app.name_any();
                if app.labels().get(PROJECT_LABEL).map(String::as_str) != Some(project) {
                    failed.push(name);
                    continue;
                }
                let mut markers = BTreeMap::from([(usage::marker(id), usage::revision(&secret))]);
                for connection in &connections {
                    if references(
                        &serde_json::to_value(&app.spec).unwrap_or_default(),
                        &connection.name_any(),
                    ) {
                        if description(connection).is_err() {
                            failed.push(name.clone());
                            continue 'apps;
                        }
                        markers.insert(
                            usage::marker(&connection.name_any()),
                            usage::revision(connection),
                        );
                    }
                }
                markers.insert(
                    "kubeweasel.io/connection-restart".into(),
                    format!(
                        "{id}:{version}:{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    ),
                );
                let patch = Patch::Merge(
                    json!({"metadata":{"uid":app.metadata.uid,"resourceVersion":app.metadata.resource_version},"spec":{"template":{"metadata":{"annotations":markers}}}}),
                );
                if async {
                    workloads::managed(s, &name)
                        .await?
                        .patch(&name, &PatchParams::default(), &patch)
                        .await
                }
                .await
                .is_ok()
                {
                    restarted.push(name);
                } else {
                    failed.push(name);
                }
            }
        }
    } else {
        for (app, (_, env)) in s.settings.connections.bindings.read().await.iter() {
            if env.iter().any(|e| refers(e, id).is_some()) {
                let mut applied = s.settings.connections.applied.write().await;
                let markers = applied.entry(app.clone()).or_default();
                for connection in &connections {
                    if env
                        .iter()
                        .any(|e| refers(e, &connection.name_any()).is_some())
                    {
                        markers.insert(connection.name_any(), usage::revision(connection));
                    }
                }
                restarted.push(app.clone());
            }
        }
    }
    Ok(
        json!({"restarted":restarted,"failed":failed,"note":"Restart requested; rollout health is reported separately. Changing a stored database password does not change the password inside an existing database."}),
    )
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
