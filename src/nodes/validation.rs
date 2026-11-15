use crate::error::{Result, invalid};
use k8s_openapi::api::core::v1::{Node, Pod};
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Deserialize)]
pub(super) struct Change {
    pub uid: String,
    pub resource_version: String,
    pub action: String,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub value: String,
    #[serde(default)]
    pub effect: String,
}
fn name_part(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
}
fn key_valid(s: &str) -> bool {
    let parts: Vec<_> = s.split('/').collect();
    match parts.as_slice() {
        [name] => name_part(name),
        [prefix, name] => {
            prefix.len() <= 253
                && prefix.split('.').all(crate::validation::valid_name)
                && name_part(name)
        }
        _ => false,
    }
}
pub(super) fn reserved(key: &str) -> bool {
    key.split_once('/').is_some_and(|(p, _)| {
        p == "kubernetes.io"
            || p.ends_with(".kubernetes.io")
            || p == "k8s.io"
            || p.ends_with(".k8s.io")
    })
}
pub(super) fn checked(node: &Node, change: &Change) -> Result<()> {
    if node.metadata.uid.as_deref() != Some(&change.uid)
        || node.metadata.resource_version.as_deref() != Some(&change.resource_version)
    {
        return Err(crate::error::Error::conflict(
            "Node changed. Refresh it and review the action again.",
        ));
    }
    Ok(())
}
pub(super) fn patch(node: &Node, c: &Change) -> Result<Value> {
    checked(node, c)?;
    match c.action.as_str() {
        "cordon" | "uncordon" => Ok(
            json!({"metadata":{"uid":c.uid,"resourceVersion":c.resource_version},"spec":{"unschedulable":c.action=="cordon"}}),
        ),
        "set-label" | "remove-label" | "set-taint" | "remove-taint" => {
            if !key_valid(&c.key)
                || reserved(&c.key)
                || (!c.value.is_empty() && !name_part(&c.value))
            {
                return Err(invalid(
                    "Use a valid custom label/taint key and a value up to 63 characters. Kubernetes system keys are read-only.",
                ));
            }
            if c.action.ends_with("label") {
                return Ok(
                    json!({"metadata":{"uid":c.uid,"resourceVersion":c.resource_version,"labels":{c.key.clone():if c.action=="remove-label"{Value::Null}else{json!(c.value)}}}}),
                );
            }
            if !matches!(
                c.effect.as_str(),
                "NoSchedule" | "PreferNoSchedule" | "NoExecute"
            ) {
                return Err(invalid("Choose a valid taint effect."));
            }
            let mut taints = serde_json::to_value(
                node.spec
                    .as_ref()
                    .and_then(|s| s.taints.clone())
                    .unwrap_or_default(),
            )
            .unwrap_or(json!([]))
            .as_array()
            .cloned()
            .unwrap_or_default();
            taints.retain(|t| t["key"] != c.key || t["effect"] != c.effect);
            if c.action == "set-taint" {
                taints.push(json!({"key":c.key,"value":c.value,"effect":c.effect}));
            }
            Ok(
                json!({"metadata":{"uid":c.uid,"resourceVersion":c.resource_version},"spec":{"taints":taints}}),
            )
        }
        _ => Err(invalid("Unknown node action.")),
    }
}
pub(super) fn pod_disposition(p: &Pod) -> (&'static str, &'static str) {
    if p.metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"))
    {
        return ("skip", "Static mirror pod: managed by the node.");
    }
    if p.metadata.owner_references.as_ref().is_some_and(|o| {
        o.iter()
            .any(|o| o.controller == Some(true) && o.kind == "DaemonSet")
    }) {
        return ("skip", "DaemonSet pod: stays on the node.");
    }
    if p.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .is_some_and(|s| s == "Succeeded" || s == "Failed")
    {
        return ("skip", "Completed pod.");
    }
    if !p
        .metadata
        .owner_references
        .as_ref()
        .is_some_and(|o| o.iter().any(|o| o.controller == Some(true)))
    {
        return ("blocked", "Unmanaged pod: it may not be recreated.");
    }
    if !p.metadata.owner_references.as_ref().is_some_and(|refs| {
        refs.iter().any(|o| {
            o.controller == Some(true)
                && matches!(
                    o.kind.as_str(),
                    "ReplicaSet" | "StatefulSet" | "Job" | "ReplicationController"
                )
        })
    }) {
        return (
            "blocked",
            "Controller type is not supported for dashboard drain. Use kubectl to review it.",
        );
    }
    if p.spec
        .as_ref()
        .and_then(|s| s.volumes.as_ref())
        .is_some_and(|v| {
            v.iter()
                .any(|v| v.empty_dir.is_some() || v.host_path.is_some() || v.ephemeral.is_some())
        })
    {
        return (
            "blocked",
            "Local or ephemeral storage: move its data before draining.",
        );
    }
    (
        "evict",
        "Evict through Kubernetes; disruption budgets and graceful termination apply.",
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edits_preserve_system_metadata_and_reject_stale_nodes() {
        let n:Node=serde_json::from_value(json!({"metadata":{"uid":"u","resourceVersion":"1"},"spec":{"taints":[{"key":"node.kubernetes.io/not-ready","effect":"NoSchedule"}]}})).unwrap();
        let mut c = Change {
            uid: "u".into(),
            resource_version: "1".into(),
            action: "set-taint".into(),
            key: "example.com/team".into(),
            value: "a".into(),
            effect: "NoSchedule".into(),
        };
        let p = patch(&n, &c).unwrap();
        assert_eq!(p["spec"]["taints"].as_array().unwrap().len(), 2);
        c.key = "node.kubernetes.io/not-ready".into();
        assert!(patch(&n, &c).is_err());
        c.action = "cordon".into();
        c.resource_version = "stale".into();
        assert!(patch(&n, &c).is_err());
    }
    #[test]
    fn drain_classifies_data_loss_and_unmanaged_pods() {
        let base = json!({"metadata":{"name":"test","ownerReferences":[{"apiVersion":"apps/v1","kind":"ReplicaSet","name":"web","uid":"owner","controller":true}]},"spec":{"containers":[{"name":"web"}]}});
        let p: Pod = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(pod_disposition(&p).0, "evict");
        for volume in [
            json!({"name":"data","emptyDir":{}}),
            json!({"name":"data","hostPath":{"path":"/data"}}),
            json!({"name":"data","ephemeral":{}}),
        ] {
            let mut v = base.clone();
            v["spec"]["volumes"] = json!([volume]);
            let p: Pod = serde_json::from_value(v).unwrap();
            assert_eq!(pod_disposition(&p).0, "blocked");
        }
        let p: Pod = serde_json::from_value(json!({"metadata":{"name":"unmanaged"}})).unwrap();
        assert_eq!(pod_disposition(&p).0, "blocked");
        let mut daemon = base;
        daemon["metadata"]["ownerReferences"][0]["kind"] = json!("DaemonSet");
        let p: Pod = serde_json::from_value(daemon).unwrap();
        assert_eq!(pod_disposition(&p).0, "skip");
    }
}
