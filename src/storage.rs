#[cfg(test)]
use crate::{
    applications::{
        self,
        models::{Deploy, Scale},
    },
    projects, settings,
};
use crate::{
    error::{Error, Result, invalid},
    http::request::mutation,
    state::AppState,
    validation::valid_name,
    workloads::{self, managed},
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use k8s_openapi::api::{
    core::v1::{PersistentVolumeClaim, Pod},
    storage::v1::StorageClass,
};
use kube::{
    Api, ResourceExt,
    api::{ListParams, Patch, PatchParams, PostParams},
};
#[cfg(test)]
use kube::{Client, api::LogParams};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct StorageState {
    pub gate: Arc<tokio::sync::Mutex<()>>,
    pub demo: Arc<RwLock<BTreeMap<String, Vec<Value>>>>,
}
#[derive(Deserialize)]
struct Mount {
    claim_name: String,
    mount_path: String,
    #[serde(default)]
    create_new: bool,
    #[serde(default)]
    size_gib: u32,
    #[serde(default)]
    storage_class: String,
}
#[derive(Deserialize)]
struct Detach {
    volume_name: String,
}
pub fn has_pvc(object: &Value) -> bool {
    object
        .pointer("/spec/template/spec/volumes")
        .and_then(Value::as_array)
        .is_some_and(|v| {
            v.iter().any(|v| {
                v.get("persistentVolumeClaim").is_some()
                    && v.get("$patch").and_then(Value::as_str) != Some("delete")
            })
        })
}
fn validate_mount(m: &Mount) -> Result<()> {
    if m.claim_name.len() > 253
        || !m.claim_name.split('.').all(valid_name)
        || !m.mount_path.starts_with('/')
        || m.mount_path == "/"
        || m.mount_path.contains("//")
        || m.mount_path.ends_with('/')
        || m.mount_path.len() > 1024
        || m.mount_path.split('/').any(|v| v == ".." || v == ".")
        || m.mount_path.chars().any(char::is_control)
        || (m.create_new && !(1..=4096).contains(&m.size_gib))
        || (!m.storage_class.is_empty()
            && (!m.storage_class.split('.').all(valid_name) || m.storage_class.len() > 253))
    {
        return Err(invalid(
            "Use a lowercase claim name, an absolute non-root mount path without . or .., and a size of 1–4096 GiB.",
        ));
    }
    Ok(())
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/{name}/storage", get(inspect).post(attach))
        .route("/api/apps/{name}/storage/detach", post(detach))
}
fn pvc_summary(pvc: &PersistentVolumeClaim) -> Value {
    json!({"name":pvc.name_any(),"phase":pvc.status.as_ref().and_then(|s|s.phase.as_deref()).unwrap_or("Pending"),"size":pvc.spec.as_ref().and_then(|s|s.resources.as_ref()).and_then(|r|r.requests.as_ref()).and_then(|r|r.get("storage")).map(|q|q.0.clone()),"storage_class":pvc.spec.as_ref().and_then(|s|s.storage_class_name.clone()),"access_modes":pvc.spec.as_ref().and_then(|s|s.access_modes.clone())})
}
async fn inspect(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    if s.client.is_none() {
        if !s.apps.read().await.contains_key(&name) {
            return Err(Error(StatusCode::NOT_FOUND, "App not found".into()));
        }
        return Ok(Json(
            json!({"demo":true,"mounts":s.storage.demo.read().await.get(&name).cloned().unwrap_or_default(),"claims":[],"classes":[],"class_discovery":false}),
        ));
    }
    let workload = managed(&s, &name).await?;
    let d = workload.get(&name).await?;
    let spec = d
        .spec
        .and_then(|s| s.template.spec)
        .ok_or_else(|| invalid("Workload has no pod specification"))?;
    let claims: Api<PersistentVolumeClaim> =
        Api::namespaced(s.client.clone().unwrap(), &s.namespace);
    let all = claims.list(&ListParams::default()).await?;
    let mut mounts = Vec::new();
    if let Some(c) = spec.containers.iter().find(|c| c.name == "app") {
        for mount in c.volume_mounts.as_ref().into_iter().flatten() {
            if let Some(volume) = spec
                .volumes
                .as_ref()
                .into_iter()
                .flatten()
                .find(|v| v.name == mount.name)
                && let Some(pvc) = volume.persistent_volume_claim.as_ref()
            {
                let mut row = all
                    .items
                    .iter()
                    .find(|p| p.name_any() == pvc.claim_name)
                    .map(pvc_summary)
                    .unwrap_or(json!({"name":pvc.claim_name,"phase":"Missing"}));
                row["mount_path"] = json!(mount.mount_path);
                row["volume_name"] = json!(mount.name);
                row["detachable"] = json!(mount.name.starts_with("kw-data-"));
                mounts.push(row);
            }
        }
    }
    let classes: Api<StorageClass> = Api::all(s.client.clone().unwrap());
    let classes = classes.list(&ListParams::default()).await;
    let discovery = classes.is_ok();
    Ok(Json(
        json!({"demo":false,"workload_kind":workload.kind,"mounts":mounts,"claims":all.items.iter().map(pvc_summary).collect::<Vec<_>>(),"classes":classes.map(|v|v.items.into_iter().map(|c|json!({"name":c.name_any(),"default":c.annotations().get("storageclass.kubernetes.io/is-default-class").map(String::as_str)==Some("true"),"binding_mode":c.volume_binding_mode})).collect::<Vec<_>>()).unwrap_or_default(),"class_discovery":discovery}),
    ))
}
async fn attach(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(m): Json<Mount>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    validate_mount(&m)?;
    let _gate = s.storage.gate.lock().await;
    let mut random = [0u8; 6];
    OsRng.fill_bytes(&mut random);
    let volume = format!(
        "kw-data-{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    if let Some(client) = s.client.clone() {
        let workload = managed(&s, &name).await?;
        let d = workload.get(&name).await?;
        let spec = d
            .spec
            .as_ref()
            .ok_or_else(|| invalid("Missing workload spec"))?;
        if spec.replicas.unwrap_or(1) > 1 {
            return Err(invalid(
                "Scale to one or zero replicas before attaching storage.",
            ));
        }
        let pod = spec
            .template
            .spec
            .as_ref()
            .ok_or_else(|| invalid("Missing pod spec"))?;
        let c = pod
            .containers
            .iter()
            .find(|c| c.name == "app")
            .ok_or_else(|| invalid("App container not found"))?;
        if c.volume_mounts.as_ref().is_some_and(|v| {
            v.len() >= 8
                || v.iter().any(|v| {
                    v.mount_path.trim_end_matches('/') == m.mount_path.trim_end_matches('/')
                })
        }) {
            return Err(invalid(
                "Mount path is already used, or the app has reached the eight-volume limit.",
            ));
        }
        // Refuse to share a database claim with another workload or active pod.
        for (_, other) in workloads::list(client.clone(), &s.namespace, "").await? {
            if other.name_any() != name
                && other
                    .spec
                    .and_then(|s| s.template.spec)
                    .and_then(|s| s.volumes)
                    .is_some_and(|vs| {
                        vs.iter().any(|v| {
                            v.persistent_volume_claim
                                .as_ref()
                                .is_some_and(|v| v.claim_name == m.claim_name)
                        })
                    })
            {
                return Err(invalid(
                    "This claim is already attached to another application.",
                ));
            }
        }
        let pods: Api<Pod> = Api::namespaced(client.clone(), &s.namespace);
        for p in pods.list(&ListParams::default()).await?.items {
            if (p.labels().get("app.kubernetes.io/name").map(String::as_str) != Some(&name)
                || p.labels()
                    .get("app.kubernetes.io/managed-by")
                    .map(String::as_str)
                    != Some("kubeweasel"))
                && p.status
                    .as_ref()
                    .and_then(|s| s.phase.as_deref())
                    .is_some_and(|v| v != "Succeeded" && v != "Failed")
                && p.spec.and_then(|s| s.volumes).is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.persistent_volume_claim
                            .as_ref()
                            .is_some_and(|v| v.claim_name == m.claim_name)
                    })
                })
            {
                return Err(invalid("This claim is mounted by another active pod."));
            }
        }
        let claims: Api<PersistentVolumeClaim> = Api::namespaced(client, &s.namespace);
        if m.create_new {
            let mut value = json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":m.claim_name,"labels":{"app.kubernetes.io/managed-by":"kubeweasel"}},"spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":format!("{}Gi",m.size_gib)}}}});
            if !m.storage_class.is_empty() {
                value["spec"]["storageClassName"] = json!(m.storage_class);
            }
            let claim = serde_json::from_value(value).map_err(|_| invalid("Invalid claim"))?;
            claims.create(&PostParams::default(), &claim).await?;
        } else {
            let claim = claims.get(&m.claim_name).await?;
            if claim.metadata.deletion_timestamp.is_some()
                || claim.spec.as_ref().and_then(|s| s.volume_mode.as_deref()) == Some("Block")
            {
                return Err(invalid(
                    "Choose an existing filesystem claim that is not being deleted.",
                ));
            }
        }
        let mut patch = json!({"spec":{"template":{"spec":{"volumes":[{"name":volume,"persistentVolumeClaim":{"claimName":m.claim_name}}],"containers":[{"name":"app","volumeMounts":[{"name":volume,"mountPath":m.mount_path}]}]}}}});
        if workload.kind == "Deployment" {
            patch["spec"]["strategy"] = json!({"type":"Recreate","rollingUpdate":null});
        }
        if let Err(e) = workload
            .patch_locked(&name, &PatchParams::default(), &Patch::Strategic(patch))
            .await
        {
            return Err(Error(
                StatusCode::BAD_GATEWAY,
                format!(
                    "Claim '{}' is retained, but mounting it failed: {}. Retry using Attach existing volume.",
                    m.claim_name, e.1
                ),
            ));
        }
    } else {
        let apps = s.apps.read().await;
        let a = apps
            .get(&name)
            .ok_or_else(|| Error(StatusCode::NOT_FOUND, "App not found".into()))?;
        if a.replicas > 1 {
            return Err(invalid(
                "Scale to one or zero replicas before attaching storage.",
            ));
        }
        let mut mounts = s.storage.demo.write().await;
        let list = mounts.entry(name).or_default();
        if list.len() >= 8 || list.iter().any(|v| v["mount_path"] == m.mount_path) {
            return Err(invalid(
                "Mount path is already used or volume limit reached.",
            ));
        }
        list.push(json!({"name":m.claim_name,"phase":"Demo only","size":format!("{}Gi",m.size_gib),"mount_path":m.mount_path,"volume_name":volume,"detachable":true}));
    }
    Ok(Json(
        json!({"ok":true,"message":"Volume attached. Pods will restart; the claim is retained independently."}),
    ))
}
async fn detach(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(d): Json<Detach>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.storage.gate.lock().await;
    if s.client.is_some() {
        let workload = managed(&s, &name).await?;
        let current = workload.get(&name).await?;
        let spec = current
            .spec
            .and_then(|s| s.template.spec)
            .ok_or_else(|| invalid("No pod specification"))?;
        let mount = spec
            .containers
            .iter()
            .find(|c| c.name == "app")
            .and_then(|c| c.volume_mounts.as_ref())
            .and_then(|ms| ms.iter().find(|m| m.name == d.volume_name))
            .ok_or_else(|| invalid("Volume mount not found"))?;
        if !d.volume_name.starts_with("kw-data-")
            || !spec.volumes.as_ref().is_some_and(|v| {
                v.iter()
                    .any(|v| v.name == d.volume_name && v.persistent_volume_claim.is_some())
            })
        {
            return Err(invalid("This mount is managed outside KubeWeasel Storage."));
        }
        if spec.containers.iter().filter(|c| c.name != "app").any(|c| {
            c.volume_mounts
                .as_ref()
                .is_some_and(|ms| ms.iter().any(|m| m.name == d.volume_name))
        }) {
            return Err(invalid(
                "Volume is shared with another container; detach it outside this editor.",
            ));
        }
        workload.patch_locked(&name,&PatchParams::default(),&Patch::Strategic(json!({"spec":{"template":{"spec":{"volumes":[{"name":d.volume_name,"$patch":"delete"}],"containers":[{"name":"app","volumeMounts":[{"mountPath":mount.mount_path,"$patch":"delete"}]}]}}}}))).await?;
    } else {
        let mut mounts = s.storage.demo.write().await;
        let list = mounts
            .get_mut(&name)
            .ok_or_else(|| invalid("Volume mount not found"))?;
        list.retain(|v| v["volume_name"] != d.volume_name);
    }
    Ok(Json(
        json!({"ok":true,"message":"Detached. The persistent volume claim and its data were not deleted."}),
    ))
}
pub async fn demo_limit(s: &AppState, name: &str, replicas: i32) -> Result<()> {
    if s.client.is_none() && replicas > 1 {
        let stateful = s
            .apps
            .read()
            .await
            .get(name)
            .is_some_and(|a| a.workload_kind == "StatefulSet");
        if stateful
            || s.storage
                .demo
                .read()
                .await
                .get(name)
                .is_some_and(|v| !v.is_empty())
        {
            return Err(invalid(
                "Storage-backed apps and StatefulSets support at most one replica.",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mount_validation() {
        let mut m = Mount {
            claim_name: "db-data".into(),
            mount_path: "/data".into(),
            create_new: true,
            size_gib: 10,
            storage_class: String::new(),
        };
        assert!(validate_mount(&m).is_ok());
        for path in ["/", "data", "/data/../etc"] {
            m.mount_path = path.into();
            assert!(validate_mount(&m).is_err());
        }
        m.mount_path = "/data".into();
        m.size_gib = 0;
        assert!(validate_mount(&m).is_err());
    }
    #[test]
    fn pvc_detection() {
        assert!(has_pvc(
            &json!({"spec":{"template":{"spec":{"volumes":[{"persistentVolumeClaim":{"claimName":"data"}}]}}}})
        ));
        assert!(!has_pvc(&json!({"spec":{"replicas":1}})));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::DeleteParams;
    async fn ready_pod(
        pods: &Api<Pod>,
        old: Option<&str>,
    ) -> std::result::Result<Pod, Box<dyn std::error::Error>> {
        for _ in 0..120 {
            for p in pods
                .list(&ListParams::default().labels("app.kubernetes.io/name=storage-check"))
                .await?
                .items
            {
                if p.metadata.deletion_timestamp.is_none()
                    && p.metadata.uid.as_deref() != old
                    && p.status
                        .as_ref()
                        .and_then(|s| s.conditions.as_ref())
                        .is_some_and(|cs| {
                            cs.iter().any(|c| c.type_ == "Ready" && c.status == "True")
                        })
                {
                    return Ok(p);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Err("Timed out waiting for ready storage test pod".into())
    }
    async fn assert_data(
        pods: &Api<Pod>,
        name: &str,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let mut last = String::new();
        for _ in 0..15 {
            last = pods.logs(name, &LogParams::default()).await?;
            if last.trim() == "kubeweasel-persistent-data" {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Err(format!("Unexpected stored file output from {name}: {last:?}").into())
    }
    #[tokio::test]
    #[ignore = "creates PVCs and writes a test file in an isolated namespace; needs a default StorageClass"]
    async fn data_survives_pod_replacement_and_detach() {
        assert_eq!(
            std::env::var("KUBEWEASEL_RUN_STORAGE_TEST").as_deref(),
            Ok("1"),
            "Explicitly enable the live storage test."
        );
        for kind in ["StatefulSet", "Deployment"] {
            storage_round_trip(kind).await;
        }
    }
    async fn storage_round_trip(kind: &str) {
        let client = Client::try_default().await.unwrap();
        let mut random = [0u8; 6];
        OsRng.fill_bytes(&mut random);
        let ns = format!(
            "kw-storage-check-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let namespace: Namespace = serde_json::from_value(
            json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":ns}}),
        )
        .unwrap();
        namespaces
            .create(&PostParams::default(), &namespace)
            .await
            .unwrap();
        let result: std::result::Result<(), Box<dyn std::error::Error>> = async {
 let s=AppState{build_gate:Default::default(),client:Some(client.clone()),namespace:ns.clone(),apps:Arc::new(RwLock::new(BTreeMap::new())),settings:settings::SettingsState::default(),storage:StorageState::default(),projects:projects::Store::open(":memory:")?};let mut headers=HeaderMap::new();headers.insert("x-kubeweasel-request","dashboard".parse().unwrap());applications::service::deploy(&s, Deploy{workload_kind:kind.into(),project_id:None,name:"storage-check".into(),image:"busybox:1.37".into(),port:8080,replicas:0}).await.map_err(|e|e.1)?;
 let workload=managed(&s,"storage-check").await.map_err(|e|e.1)?;workload.patch("storage-check",&PatchParams::default(),&Patch::Strategic(json!({"spec":{"template":{"spec":{"terminationGracePeriodSeconds":1,"containers":[{"name":"app","command":["sh","-c","echo kubeweasel-persistent-data > /data/probe; cat /data/probe; sleep 3600"]}]}}}}))).await.map_err(|e|e.1)?;
 let _=attach(State(s.clone()),Path("storage-check".into()),headers.clone(),Json(Mount{claim_name:"test-data".into(),mount_path:"/data".into(),create_new:true,size_gib:1,storage_class:String::new()})).await.map_err(|e|e.1)?;
 if applications::service::scale(&s, "storage-check", Scale{replicas:2}).await.is_ok(){return Err("Unsafe stateful scaling was allowed".into());}
 applications::service::scale(&s, "storage-check", Scale{replicas:1}).await.map_err(|e|e.1)?;let pods:Api<Pod>=Api::namespaced(client.clone(),&ns);let first=ready_pod(&pods,None).await?;
 assert_data(&pods,&first.name_any()).await?;
 workload.patch("storage-check",&PatchParams::default(),&Patch::Strategic(json!({"spec":{"template":{"spec":{"containers":[{"name":"app","command":["sh","-c","cat /data/probe; sleep 3600"]}]}}}}))).await.map_err(|e|e.1)?;let next=ready_pod(&pods,first.metadata.uid.as_deref()).await?;assert_data(&pods,&next.name_any()).await?;
 let config=serde_json::to_value(settings::load(&s,"storage-check").await.map_err(|e|e.1)?)?;if config["workload_kind"]!=kind||config["max_replicas"]!=1{return Err("Stateful configuration readback failed".into());}
 applications::service::scale(&s, "storage-check", Scale{replicas:0}).await.map_err(|e|e.1)?;let storage=inspect(State(s.clone()),Path("storage-check".into())).await.map_err(|e|e.1)?.0;let volume=storage["mounts"][0]["volume_name"].as_str().unwrap().to_string();let _=detach(State(s.clone()),Path("storage-check".into()),headers.clone(),Json(Detach{volume_name:volume})).await.map_err(|e|e.1)?;let claims:Api<PersistentVolumeClaim>=Api::namespaced(client.clone(),&ns);let retained=claims.get("test-data").await?;if retained.metadata.owner_references.is_some(){return Err("Claim must be retained independently".into());}
 let _=attach(State(s.clone()),Path("storage-check".into()),headers.clone(),Json(Mount{claim_name:"test-data".into(),mount_path:"/data".into(),create_new:false,size_gib:0,storage_class:String::new()})).await.map_err(|e|e.1)?;applications::service::scale(&s, "storage-check", Scale{replicas:1}).await.map_err(|e|e.1)?;let restored=ready_pod(&pods,next.metadata.uid.as_deref()).await?;assert_data(&pods,&restored.name_any()).await?;Ok(())
 }.await;
        namespaces
            .delete(&ns, &DeleteParams::default())
            .await
            .unwrap();
        result.unwrap();
    }
}
