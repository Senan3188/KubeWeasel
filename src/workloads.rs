use super::*;
use kube::api::{ApiResource, DynamicObject, GroupVersionKind};
#[derive(Clone)]
pub struct Managed {
    api: Api<DynamicObject>,
    pub kind: String,
    gate: Arc<tokio::sync::Mutex<()>>,
}
fn api(client: Client, ns: &str, kind: &str) -> Api<DynamicObject> {
    Api::namespaced_with(
        client,
        ns,
        &ApiResource::from_gvk(&GroupVersionKind::gvk("apps", "v1", kind)),
    )
}
fn normalize(mut object: Value) -> Result<Deployment> {
    object["kind"] = json!("Deployment");
    serde_json::from_value(object).map_err(|_| invalid("Cannot read workload configuration."))
}
fn owned(object: &DynamicObject) -> Result<()> {
    if object
        .labels()
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some("kubeweasel")
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Workload is not managed by KubeWeasel.".into(),
        ));
    }
    Ok(())
}
pub async fn managed(s: &AppState, name: &str) -> Result<Managed> {
    let client = s.client.clone().ok_or_else(|| invalid("No cluster"))?;
    for kind in ["Deployment", "StatefulSet"] {
        let api = api(client.clone(), &s.namespace, kind);
        if let Some(object) = api.get_opt(name).await? {
            owned(&object)?;
            return Ok(Managed {
                api,
                kind: kind.into(),
                gate: s.storage.gate.clone(),
            });
        }
    }
    Err(Error(
        StatusCode::NOT_FOUND,
        "Application not found.".into(),
    ))
}
// The Deployment controller updates its revision annotation during rollout.
// It is not an owner edit and can safely coexist with a status-conflict retry.
fn edit_annotations(object: &DynamicObject) -> BTreeMap<String, String> {
    object
        .annotations()
        .iter()
        .filter(|(k, _)| k.as_str() != "deployment.kubernetes.io/revision")
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
impl Managed {
    pub async fn get(&self, name: &str) -> Result<Deployment> {
        let object = self.api.get(name).await?;
        owned(&object)?;
        normalize(serde_json::to_value(object).map_err(|_| invalid("Invalid workload"))?)
    }
    pub async fn patch<T: Serialize>(
        &self,
        name: &str,
        params: &PatchParams,
        patch: &Patch<T>,
    ) -> Result<Deployment> {
        let _gate = self.gate.lock().await;
        self.patch_locked(name, params, patch).await
    }
    pub async fn patch_locked<T: Serialize>(
        &self,
        name: &str,
        params: &PatchParams,
        patch: &Patch<T>,
    ) -> Result<Deployment> {
        let base = self.api.get(name).await?;
        owned(&base)?;
        let original_value = match patch {
            Patch::Merge(v) | Patch::Strategic(v) | Patch::Apply(v) => {
                serde_json::to_value(v).map_err(|_| invalid("Invalid workload patch"))?
            }
            _ => return Err(invalid("Unsupported workload patch")),
        };
        let mut current = base.clone();
        for attempt in 0..4 {
            let mut value = original_value.clone();
            let existing =
                serde_json::to_value(&current).map_err(|_| invalid("Invalid workload"))?;
            let replicas = value
                .pointer("/spec/replicas")
                .and_then(Value::as_i64)
                .or_else(|| existing.pointer("/spec/replicas").and_then(Value::as_i64))
                .unwrap_or(1);
            let persistent = storage::has_pvc(&existing) || storage::has_pvc(&value);
            if replicas > 1 && (self.kind == "StatefulSet" || persistent) {
                return Err(invalid(
                    "Persistent-storage applications and StatefulSets support at most one replica. Database replication needs separate configuration.",
                ));
            }
            if let Some(expected) = original_value
                .pointer("/metadata/resourceVersion")
                .and_then(Value::as_str)
                && Some(expected) != current.metadata.resource_version.as_deref()
            {
                return Err(invalid("Application changed; refresh and retry."));
            }
            history::enrich(&existing, &mut value);
            value["metadata"]["resourceVersion"] = json!(current.metadata.resource_version);
            let updated = match patch {
                Patch::Strategic(_) => self.api.patch(name, params, &Patch::Strategic(value)).await,
                Patch::Merge(_) => self.api.patch(name, params, &Patch::Merge(value)).await,
                _ => self.api.patch(name, params, &Patch::Apply(value)).await,
            };
            match updated {
                Ok(updated) => {
                    return normalize(
                        serde_json::to_value(updated).map_err(|_| invalid("Invalid workload"))?,
                    );
                }
                Err(kube::Error::Api(ref error)) if error.code == 409 && attempt < 3 => {
                    let fresh = self.api.get(name).await?;
                    owned(&fresh)?;
                    // Controllers update status frequently. Retry only if the desired
                    // workload and identity have not changed since our first read.
                    if fresh.data.get("spec") != base.data.get("spec")
                        || fresh.metadata.uid != base.metadata.uid
                        || fresh.metadata.labels != base.metadata.labels
                        || edit_annotations(&fresh) != edit_annotations(&base)
                        || fresh.metadata.deletion_timestamp != base.metadata.deletion_timestamp
                    {
                        return Err(invalid(
                            "Application changed during this update. Refresh and retry.",
                        ));
                    }
                    current = fresh;
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(invalid("Application is busy. Retry this update."))
    }
}
pub async fn list(client: Client, ns: &str, selector: &str) -> Result<Vec<(String, Deployment)>> {
    let mut result = Vec::new();
    for kind in ["Deployment", "StatefulSet"] {
        for object in api(client.clone(), ns, kind)
            .list(&ListParams::default().labels(selector))
            .await?
            .items
        {
            result.push((
                kind.into(),
                normalize(serde_json::to_value(object).map_err(|_| invalid("Invalid workload"))?)?,
            ));
        }
    }
    Ok(result)
}
pub async fn name_available(client: Client, ns: &str, name: &str) -> Result<()> {
    for kind in ["Deployment", "StatefulSet"] {
        if api(client.clone(), ns, kind).get_opt(name).await?.is_some() {
            return Err(Error(
                StatusCode::CONFLICT,
                "An application with this name already exists.".into(),
            ));
        }
    }
    Ok(())
}
pub async fn create(client: Client, ns: &str, kind: &str, mut value: Value) -> Result<()> {
    value["kind"] = json!(kind);
    if kind == "StatefulSet" {
        value["spec"]["serviceName"] = value["metadata"]["name"].clone();
        value["spec"]["persistentVolumeClaimRetentionPolicy"] =
            json!({"whenDeleted":"Retain","whenScaled":"Retain"});
    }
    let object: DynamicObject =
        serde_json::from_value(value).map_err(|_| invalid("Invalid workload"))?;
    api(client, ns, kind)
        .create(&PostParams::default(), &object)
        .await?;
    Ok(())
}
