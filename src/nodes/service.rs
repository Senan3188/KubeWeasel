use super::validation::{self, Change};
use crate::{
    error::{Error, Result, invalid},
    state::AppState,
};
use axum::http::StatusCode;
use k8s_openapi::api::{
    authorization::v1::{ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec},
    core::v1::{Event, Node, Pod},
    policy::v1::Eviction,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{DeleteOptions, ObjectMeta, Preconditions};
use kube::{
    Api, Client, ResourceExt,
    api::{ListParams, Patch, PatchParams, PostParams},
};
use serde_json::{Value, json};
use std::time::Duration;

pub(super) fn client(s: &AppState) -> Result<Client> {
    s.client.clone().ok_or_else(|| {
        invalid("Node administration requires a live cluster. Demo mode never changes nodes.")
    })
}
fn failure(e: kube::Error) -> Error {
    match &e{kube::Error::Api(a) if a.code==403=>Error(StatusCode::FORBIDDEN,"This cluster connection lacks node administration permissions. Ask an administrator for the optional node-management role.".into()),kube::Error::Api(a) if a.code==404=>Error::not_found("Node or pod no longer exists. Refresh the page."),kube::Error::Api(a) if a.code==409=>Error::conflict("Resource changed. Refresh and review the action again."),_=>Error::bad_gateway("The cluster could not complete the node request. Check its connection and permissions.")}
}
async fn bounded<T>(
    future: impl std::future::Future<Output = std::result::Result<T, kube::Error>>,
) -> Result<T> {
    tokio::time::timeout(Duration::from_secs(8), future)
        .await
        .map_err(|_| {
            Error::bad_gateway(
                "The node request timed out. Refresh to inspect the current state before retrying.",
            )
        })?
        .map_err(failure)
}
pub(super) async fn permissions(client: Client, name: Option<&str>) -> Value {
    let api: Api<SelfSubjectAccessReview> = Api::all(client);
    let mut result = json!({});
    for (key, resource, subresource, verb, node_name) in [
        ("patch", "nodes", None, "patch", name),
        ("evict", "pods", Some("eviction"), "create", None),
        ("pods", "pods", None, "list", None),
    ] {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    resource: Some(resource.into()),
                    subresource: subresource.map(str::to_string),
                    verb: Some(verb.into()),
                    name: node_name.map(str::to_string),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let allowed = bounded(api.create(&PostParams::default(), &review))
            .await
            .ok()
            .and_then(|r| r.status.map(|s| s.allowed));
        result[key] = json!(allowed);
    }
    result
}
fn summary(n: &Node) -> Value {
    let status = n.status.as_ref();
    let labels = n.metadata.labels.clone().unwrap_or_default();
    let allocatable = |key: &str| {
        status
            .and_then(|s| s.allocatable.as_ref())
            .and_then(|a| a.get(key))
            .and_then(|q| crate::monitoring::quantity(&q.0, key == "cpu"))
    };
    let roles: Vec<_> = labels
        .keys()
        .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
        .collect();
    json!({"name":n.name_any(),"uid":n.metadata.uid,"resource_version":n.metadata.resource_version,"created_at":n.metadata.creation_timestamp,"labels":labels,"roles":roles,"unschedulable":n.spec.as_ref().and_then(|s|s.unschedulable).unwrap_or(false),"taints":n.spec.as_ref().and_then(|s|s.taints.clone()).unwrap_or_default(),"ready":status.and_then(|s|s.conditions.as_ref()).is_some_and(|cs|cs.iter().any(|c|c.type_=="Ready"&&c.status=="True")),"conditions":status.and_then(|s|s.conditions.clone()).unwrap_or_default(),"addresses":status.and_then(|s|s.addresses.clone()).unwrap_or_default(),"capacity":status.and_then(|s|s.capacity.clone()),"allocatable":status.and_then(|s|s.allocatable.clone()),"allocatable_cpu":allocatable("cpu"),"allocatable_memory":allocatable("memory"),"info":status.and_then(|s|s.node_info.clone())})
}
pub(super) async fn list(s: &AppState) -> Result<Value> {
    let Some(client) = s.client.clone() else {
        return Ok(
            json!({"mode":"demo","nodes":[],"message":"Connect a live cluster to view and manage its nodes."}),
        );
    };
    let api: Api<Node> = Api::all(client);
    let list = bounded(api.list(&ListParams::default().limit(1000))).await?;
    if list
        .metadata
        .continue_
        .as_ref()
        .is_some_and(|s| !s.is_empty())
    {
        return Err(invalid(
            "This view supports up to 1,000 nodes per connection.",
        ));
    }
    let mut rows: Vec<_> = list.items.iter().map(summary).collect();
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({"mode":"live","nodes":rows}))
}
async fn node(client: Client, name: &str) -> Result<Node> {
    if name.len() > 253 || !name.split('.').all(crate::validation::valid_name) {
        return Err(invalid("Invalid node name."));
    }
    bounded(Api::<Node>::all(client).get(name)).await
}
async fn pods(client: Client, name: &str) -> Result<Vec<Pod>> {
    let list = bounded(
        Api::<Pod>::all(client).list(
            &ListParams::default()
                .fields(&format!("spec.nodeName={name}"))
                .limit(501),
        ),
    )
    .await?;
    if list.items.len() > 500
        || list
            .metadata
            .continue_
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    {
        return Err(invalid(
            "This node has more than 500 pods. Use kubectl for maintenance.",
        ));
    }
    Ok(list.items)
}
fn pod_summary(p: &Pod) -> Value {
    let (action, reason) = validation::pod_disposition(p);
    json!({"name":p.name_any(),"namespace":p.namespace(),"uid":p.metadata.uid,"phase":p.status.as_ref().and_then(|s|s.phase.clone()),"terminating":p.metadata.deletion_timestamp.is_some(),"action":action,"reason":reason,"persistent_storage":p.spec.as_ref().and_then(|s|s.volumes.as_ref()).is_some_and(|vs|vs.iter().any(|v|v.persistent_volume_claim.is_some()))})
}
pub(super) async fn detail(s: &AppState, name: &str) -> Result<Value> {
    let client = client(s)?;
    let n = node(client.clone(), name).await?;
    let (pods, permissions) = tokio::join!(
        pods(client.clone(), name),
        permissions(client.clone(), Some(name))
    );
    let events = bounded(
        Api::<Event>::all(client).list(
            &ListParams::default()
                .fields(&format!(
                    "involvedObject.kind=Node,involvedObject.uid={}",
                    n.metadata.uid.as_deref().unwrap_or("")
                ))
                .limit(100),
        ),
    )
    .await;
    let (events, event_state) = match events {
        Ok(mut list) => {
            list.items
                .sort_by(|a, b| b.last_timestamp.cmp(&a.last_timestamp));
            (list.items.into_iter().take(30).map(|e|json!({"reason":e.reason,"message":e.message,"type":e.type_,"time":e.last_timestamp})).collect::<Vec<_>>(),"available")
        }
        Err(_) => (vec![], "unavailable"),
    };
    let (pod_rows, pod_error) = match pods {
        Ok(ps) => (Some(ps.iter().map(pod_summary).collect::<Vec<_>>()), None),
        Err(e) => (None, Some(e.1)),
    };
    Ok(
        json!({"node":summary(&n),"pods":pod_rows,"pods_error":pod_error,"permissions":permissions,"events":events,"events_state":event_state}),
    )
}
pub(super) async fn change(s: &AppState, name: &str, input: &Change) -> Result<Value> {
    let client = client(s)?;
    let n = node(client.clone(), name).await?;
    let patch = validation::patch(&n, input)?;
    let updated = bounded(Api::<Node>::all(client).patch(
        name,
        &PatchParams::default(),
        &Patch::Merge(patch),
    ))
    .await?;
    Ok(json!({"node":summary(&updated)}))
}
async fn verify_owners(client: Client, ps: &[Pod]) -> Result<()> {
    use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
    let mut owners = std::collections::BTreeSet::new();
    for p in ps
        .iter()
        .filter(|p| validation::pod_disposition(p).0 == "evict")
    {
        let owner = p
            .metadata
            .owner_references
            .as_ref()
            .and_then(|refs| refs.iter().find(|o| o.controller == Some(true)))
            .ok_or_else(|| invalid("Pod controller is unavailable."))?;
        if !crate::validation::valid_name(&p.namespace().unwrap_or_default())
            || owner.name.len() > 253
            || !owner.name.split('.').all(crate::validation::valid_name)
        {
            return Err(invalid("Pod controller has an unsupported name."));
        }
        let expected_version = match owner.kind.as_str() {
            "ReplicaSet" | "StatefulSet" => "apps/v1",
            "Job" => "batch/v1",
            "ReplicationController" => "v1",
            _ => return Err(invalid("Unsupported pod controller.")),
        };
        if owner.api_version != expected_version {
            return Err(invalid("Unsupported pod controller API version."));
        }
        owners.insert((
            p.namespace().unwrap_or_default(),
            owner.kind.clone(),
            owner.name.clone(),
            owner.uid.clone(),
        ));
    }
    if owners.len() > 100 {
        return Err(invalid(
            "Use kubectl to drain nodes with more than 100 workload controllers.",
        ));
    }
    let mut checks = tokio::task::JoinSet::new();
    for (namespace, kind, name, uid) in owners {
        let client = client.clone();
        checks.spawn(async move {
            let (group, plural) = match kind.as_str() {
                "ReplicaSet" => ("apps", "replicasets"),
                "StatefulSet" => ("apps", "statefulsets"),
                "Job" => ("batch", "jobs"),
                _ => ("", "replicationcontrollers"),
            };
            let resource = ApiResource::from_gvk_with_plural(
                &GroupVersionKind::gvk(group, "v1", &kind),
                plural,
            );
            let result = bounded(
                Api::<DynamicObject>::namespaced_with(client, &namespace, &resource).get(&name),
            )
            .await;
            result.is_ok_and(|o| {
                o.metadata.uid.as_deref() == Some(&uid) && o.metadata.deletion_timestamp.is_none()
            })
        });
    }
    while let Some(result) = checks.join_next().await {
        if !matches!(result, Ok(true)) {
            return Err(invalid(
                "A workload controller is missing, terminating, or cannot be read. Grant controller read permissions or resolve it before draining.",
            ));
        }
    }
    Ok(())
}
pub(super) async fn preview(s: &AppState, name: &str) -> Result<Value> {
    let client = client(s)?;
    let n = node(client.clone(), name).await?;
    let ps = pods(client.clone(), name).await?;
    let owner_error = verify_owners(client, &ps).await.err().map(|e| e.1);
    Ok(
        json!({"node":summary(&n),"pods":ps.iter().map(pod_summary).collect::<Vec<_>>(),"pod_uids":uids(&ps),"blocked":owner_error.is_some()||ps.iter().any(|p|validation::pod_disposition(p).0=="blocked"),"owner_error":owner_error,"message":"Draining cordons this node and evicts eligible pods across every namespace. Disruption budgets are respected. DaemonSet, static and completed pods remain. Local/ephemeral data and unmanaged pods block this operation. PVCs are retained, but volume/node affinity may prevent replacement pods from starting elsewhere."}),
    )
}
fn uids(ps: &[Pod]) -> Vec<String> {
    let mut uids = ps
        .iter()
        .map(|p| p.metadata.uid.clone().unwrap_or_default())
        .collect::<Vec<_>>();
    uids.sort();
    uids
}
pub(super) async fn drain(
    s: &AppState,
    name: &str,
    input: &Change,
    expected: &[String],
) -> Result<Value> {
    let client = client(s)?;
    let n = node(client.clone(), name).await?;
    validation::checked(&n, input)?;
    let ps = pods(client.clone(), name).await?;
    let mut expected = expected.to_vec();
    expected.sort();
    if uids(&ps) != expected {
        return Err(Error::conflict(
            "Pods changed since the preview. Preview the drain again.",
        ));
    }
    if ps
        .iter()
        .any(|p| validation::pod_disposition(p).0 == "blocked")
    {
        return Err(invalid(
            "Drain blocked by unmanaged pods or local/ephemeral storage. Nothing was changed.",
        ));
    }
    verify_owners(client.clone(), &ps).await?;
    // Never bypass disruption budgets, force-delete pods or shorten their grace period.
    let api: Api<Node> = Api::all(client.clone());
    bounded(api.patch(name,&PatchParams::default(),&Patch::Merge(json!({"metadata":{"uid":input.uid,"resourceVersion":input.resource_version},"spec":{"unschedulable":true}})))).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    let mut requested = vec![];
    let mut failures = vec![];
    // Re-list after cordoning: newly arrived pods require a fresh preview.
    let current = match pods(client.clone(), name).await {
        Ok(ps) => ps,
        Err(e) => {
            return Ok(
                json!({"state":"incomplete","cordoned":true,"message":e.1,"requested":[],"failures":[],"remaining":null}),
            );
        }
    };
    if uids(&current) != expected {
        return Ok(
            json!({"state":"incomplete","cordoned":true,"message":"Pods changed while cordoning. Preview again before continuing.","requested":[],"failures":[],"remaining":null}),
        );
    }
    for p in &current {
        if validation::pod_disposition(p).0 != "evict" {
            continue;
        }
        let name = p.name_any();
        let ns = p.namespace().unwrap_or_default();
        let eviction = Eviction {
            metadata: ObjectMeta {
                name: Some(name.clone()),
                namespace: Some(ns.clone()),
                ..Default::default()
            },
            delete_options: Some(DeleteOptions {
                preconditions: Some(Preconditions {
                    uid: p.metadata.uid.clone(),
                    resource_version: p.metadata.resource_version.clone(),
                }),
                ..Default::default()
            }),
        };
        let result = tokio::time::timeout_at(
            deadline.min(tokio::time::Instant::now() + Duration::from_secs(3)),
            Api::<Pod>::namespaced(client.clone(), &ns).create_subresource::<_, Value>(
                "eviction",
                &name,
                &PostParams::default(),
                &eviction,
            ),
        )
        .await;
        match result {
            Ok(Ok(_)) => requested.push(json!({"namespace":ns,"name":name})),
            Ok(Err(kube::Error::Api(e))) if e.code == 404 => {}
            other => {
                let message = match other {
                    Ok(Err(kube::Error::Api(e))) if e.code == 429 => {
                        "Disruption budget blocks eviction. Restore availability and retry."
                    }
                    Ok(Err(kube::Error::Api(e))) if e.code == 403 => "Eviction permission denied.",
                    Ok(Err(kube::Error::Api(e))) if e.code == 409 => "Pod changed. Preview again.",
                    _ => "Eviction failed or timed out; inspect the pod before retrying.",
                };
                failures.push(json!({"namespace":ns,"name":name,"message":message}));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
    }
    let remaining = pods(client, name).await;
    let remaining = remaining.ok().map(|ps| {
        ps.iter()
            .filter(|p| validation::pod_disposition(p).0 != "skip")
            .map(pod_summary)
            .collect::<Vec<_>>()
    });
    let complete = remaining.as_ref().is_some_and(Vec::is_empty) && failures.is_empty();
    Ok(
        json!({"state":if complete{"complete"}else{"incomplete"},"cordoned":true,"requested":requested,"failures":failures,"remaining":remaining,"message":if complete{"Eligible pods have left this node. Static and DaemonSet pods may remain. The node stays cordoned."}else{"The node is cordoned. Some pods remain, are terminating, or could not be verified. Refresh and preview again to retry. Do not shut the node down yet."}}),
    )
}
