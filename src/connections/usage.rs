//! Non-sensitive revision markers and workload rollout status for connection use.
use super::models::Consumer;
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Secret};
use kube::ResourceExt;
use rand_core::{OsRng, RngCore};
use std::collections::BTreeMap;

pub(super) const REVISION: &str = "kubeweasel.io/connection-revision";
pub(super) fn revision(secret: &Secret) -> String {
    secret
        .annotations()
        .get(REVISION)
        .cloned()
        .unwrap_or_else(|| {
            format!(
                "legacy:{}",
                secret
                    .metadata
                    .resource_version
                    .as_deref()
                    .unwrap_or("unknown")
            )
        })
}
pub(super) fn new_revision() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
pub(super) fn marker(id: &str) -> String {
    format!(
        "kubeweasel.io/connection-{}",
        id.strip_prefix("kw-conn-").unwrap_or(id)
    )
}
pub(super) fn consumer(kind: &str, app: &Deployment, id: &str, revision: &str) -> Consumer {
    let desired = app.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let applied = app
        .spec
        .as_ref()
        .and_then(|s| s.template.metadata.as_ref())
        .and_then(|m| m.annotations.as_ref())
        .and_then(|a| a.get(&marker(id)));
    let required = applied.map(String::as_str) != Some(revision);
    let current = app.status.as_ref().is_some_and(|s| {
        s.observed_generation
            .is_some_and(|g| g >= app.metadata.generation.unwrap_or(i64::MAX))
            && s.updated_replicas == Some(desired)
            && s.ready_replicas.unwrap_or(0) >= desired
            && s.replicas == Some(desired)
    });
    Consumer {
        name: app.name_any(),
        kind: kind.into(),
        restart_required: required,
        status: if desired == 0 {
            "Paused · applies on next start"
        } else if required {
            "Restart required"
        } else if current {
            "Current"
        } else {
            "Restart requested · rollout pending"
        }
        .into(),
    }
}
pub(super) fn demo_consumer(
    app: &str,
    kind: &str,
    paused: bool,
    id: &str,
    revision: &str,
    applied: &BTreeMap<String, String>,
) -> Consumer {
    let required = applied.get(id).map(String::as_str) != Some(revision);
    Consumer {
        name: app.into(),
        kind: kind.into(),
        restart_required: required,
        status: if paused {
            "Paused · applies on next start"
        } else if required {
            "Restart required"
        } else {
            "Current (demo)"
        }
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn warning_waits_for_current_ready_rollout_and_survives_pausing() {
        let id = "kw-conn-example";
        let mut app:Deployment=serde_json::from_value(json!({"metadata":{"name":"backend","generation":2},"spec":{"replicas":1,"selector":{},"template":{"spec":{"containers":[]}}},"status":{"observedGeneration":1,"replicas":1,"readyReplicas":1,"updatedReplicas":1}})).unwrap();
        assert!(consumer("Deployment", &app, id, "new").restart_required);
        app.spec.as_mut().unwrap().template.metadata = Some(kube::core::ObjectMeta {
            annotations: Some(BTreeMap::from([(marker(id), "new".into())])),
            ..Default::default()
        });
        assert_eq!(
            consumer("Deployment", &app, id, "new").status,
            "Restart requested · rollout pending"
        );
        app.status.as_mut().unwrap().observed_generation = Some(2);
        assert_eq!(consumer("Deployment", &app, id, "new").status, "Current");
        app.status.as_mut().unwrap().updated_replicas = Some(0);
        assert_ne!(consumer("Deployment", &app, id, "new").status, "Current");
        app.spec.as_mut().unwrap().replicas = Some(0);
        assert!(consumer("Deployment", &app, id, "changed").restart_required);
        assert!(
            consumer("Deployment", &app, id, "changed")
                .status
                .starts_with("Paused")
        );
    }
}
