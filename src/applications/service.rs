//! Application operations, independent of Axum request/response handling.
use super::{
    manifests,
    models::{App, Deploy, Scale},
};
use crate::{
    error::{Error, Result, invalid},
    projects,
    state::AppState,
    storage,
    validation::validate,
    workloads::{self, LABEL},
};
use k8s_openapi::api::core::v1::{Pod, Service};
use kube::{
    Api, ResourceExt,
    api::{ListParams, LogParams, Patch, PatchParams, PostParams},
};
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) async fn overview(state: &AppState) -> Result<Vec<App>> {
    if let Some(client) = state.client.clone() {
        Ok(workloads::list(client, &state.namespace, LABEL)
            .await?
            .into_iter()
            .map(|(kind, workload)| summarize(kind, workload))
            .collect())
    } else {
        Ok(state.apps.read().await.values().cloned().collect())
    }
}
fn summarize(kind: String, workload: k8s_openapi::api::apps::v1::Deployment) -> App {
    let spec = workload.spec.as_ref();
    let ready = workload
        .status
        .as_ref()
        .and_then(|status| status.ready_replicas)
        .unwrap_or(0);
    let replicas = spec.and_then(|spec| spec.replicas).unwrap_or(1);
    let image = spec
        .and_then(|spec| spec.template.spec.as_ref())
        .and_then(|pod| pod.containers.first())
        .and_then(|container| container.image.clone())
        .unwrap_or_default();
    let build_status = workload
        .annotations()
        .get("kubeweasel.io/build-status")
        .map(String::as_str);
    let status = match build_status {
        Some("Building") => "Building",
        Some("Failed") if replicas == 0 => "Build failed",
        _ if replicas == 0 => "Paused",
        _ if ready >= replicas => "Healthy",
        _ => "Deploying",
    };
    App {
        workload_kind: kind,
        project_id: workload.labels().get(projects::PROJECT_LABEL).cloned(),
        name: workload.name_any(),
        image,
        replicas,
        ready,
        status: status.into(),
    }
}
pub(crate) async fn deploy(state: &AppState, request: Deploy) -> Result<()> {
    deploy_with_pull_secret(state, request, None).await
}
pub(crate) async fn deploy_with_pull_secret(
    state: &AppState,
    request: Deploy,
    pull_secret: Option<String>,
) -> Result<()> {
    validate(&request)?;
    let _project_gate = state.projects.gate.lock().await;
    let _storage_gate = state.storage.gate.lock().await;
    let kind = if request.workload_kind.is_empty() {
        "Deployment".to_string()
    } else {
        request.workload_kind.clone()
    };
    state
        .projects
        .require(&state.namespace, request.project_id.as_deref())?;
    let mut labels = BTreeMap::from([(
        "app.kubernetes.io/managed-by".to_string(),
        "kubeweasel".to_string(),
    )]);
    if let Some(id) = &request.project_id {
        labels.insert(projects::PROJECT_LABEL.into(), id.clone());
    }
    if let Some(c) = state.client.clone() {
        workloads::name_available(c.clone(), &state.namespace, &request.name).await?;
        let deployment = manifests::deployment(&request, labels.clone(), pull_secret);
        workloads::create(
            c.clone(),
            &state.namespace,
            &kind,
            serde_json::to_value(&deployment).map_err(|_| invalid("Invalid workload"))?,
        )
        .await?;
        let services: Api<Service> = Api::namespaced(c, &state.namespace);
        let service = manifests::service(&request, labels, &kind);
        if let Err(e) = services.create(&PostParams::default(), &service).await {
            return Err(Error::bad_gateway(format!(
                "Deployment created, but Service creation failed: {e}. Inspect the resources before retrying."
            )));
        }
    } else {
        let mut apps = state.apps.write().await;
        if apps.contains_key(&request.name) {
            return Err(Error::conflict("An app with this name already exists."));
        }
        let app = App {
            workload_kind: kind,
            project_id: request.project_id,
            name: request.name,
            image: request.image,
            replicas: request.replicas,
            ready: request.replicas,
            status: if request.replicas == 0 {
                "Paused"
            } else {
                "Healthy"
            }
            .into(),
        };
        state.settings.initialize_demo(&app, request.port).await;
        apps.insert(app.name.clone(), app);
    }
    Ok(())
}
pub(crate) async fn scale(state: &AppState, name: &str, request: Scale) -> Result<()> {
    let _demo_gate = if state.client.is_none() {
        Some(state.storage.gate.lock().await)
    } else {
        None
    };
    storage::demo_limit(state, name, request.replicas).await?;
    if !(0..=20).contains(&request.replicas) {
        return Err(invalid("Replicas must be 0–20."));
    }
    if state.client.is_some() {
        workloads::managed(state, name)
            .await?
            .patch(
                name,
                &PatchParams::default(),
                &Patch::Merge(json!({"spec":{"replicas":request.replicas}})),
            )
            .await?;
    } else {
        let mut apps = state.apps.write().await;
        let app = apps
            .get_mut(name)
            .ok_or_else(|| Error::not_found("App not found"))?;
        app.replicas = request.replicas;
        app.ready = request.replicas;
        app.status = if request.replicas == 0 {
            "Paused"
        } else {
            "Healthy"
        }
        .into();
    }
    Ok(())
}
pub(crate) async fn restart(state: &AppState, name: &str) -> Result<()> {
    crate::connections::restart_application(state, name).await
}
pub(crate) async fn logs(state: &AppState, name: &str) -> Result<String> {
    if let Some(c) = state.client.clone() {
        workloads::managed(state, name).await?;
        let pods: Api<Pod> = Api::namespaced(c, &state.namespace);
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
        Ok(if output.is_empty() {
            "No pods available yet.".into()
        } else {
            output
        })
    } else {
        if !state.apps.read().await.contains_key(name) {
            return Err(Error::not_found("App not found"));
        }
        Ok(format!(
            "[demo] {name}: container started\n[demo] Application listening for requests\n[demo] These are sample logs, not cluster output."
        ))
    }
}
