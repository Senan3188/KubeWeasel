//! Bounded workload-local image history. No environment values or Secret contents.
use super::*;
use rand_core::{OsRng, RngCore};
const KEY: &str = "kubeweasel.io/deployment-history";
const MARK: &str = "kubeweasel.io/deployment-id";
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    time: u64,
    action: String,
    image: String,
    status: String,
    message: String,
    #[serde(default)]
    pull_secrets: Vec<String>,
    #[serde(default)]
    build: Option<String>,
    #[serde(default)]
    generation: Option<i64>,
    #[serde(default)]
    resolved_image: Option<String>,
}
fn read(v: &Value) -> Vec<Entry> {
    v.pointer("/metadata/annotations")
        .and_then(|a| a.get(KEY))
        .and_then(Value::as_str)
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or_default()
}
fn image(v: &Value) -> String {
    v.pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .and_then(|cs| cs.iter().find(|c| c["name"] == "app"))
        .and_then(|c| c["image"].as_str())
        .unwrap_or("")
        .into()
}
fn secrets(v: &Value) -> Vec<String> {
    v.pointer("/spec/template/spec/imagePullSecrets")
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(|v| v["name"].as_str().map(String::from))
                .take(16)
                .collect()
        })
        .unwrap_or_default()
}
fn annotation<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.pointer("/metadata/annotations")
        .and_then(|a| a.get(key))
        .and_then(Value::as_str)
}
fn id() -> String {
    let mut b = [0u8; 8];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|b| format!("{b:02x}")).collect()
}
fn entry(v: &Value, action: &str) -> Entry {
    Entry {
        id: id(),
        time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        action: action.into(),
        image: image(v),
        status: "Deploying".into(),
        message: "Waiting for rollout readiness.".into(),
        pull_secrets: secrets(v),
        build: None,
        generation: v.pointer("/metadata/generation").and_then(Value::as_i64),
        resolved_image: None,
    }
}
fn ready(v: &Value) -> bool {
    let desired = v
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    desired > 0
        && v.pointer("/status/observedGeneration") == v.pointer("/metadata/generation")
        && v.pointer("/status/updatedReplicas")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            >= desired
        && v.pointer("/status/readyReplicas")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            >= desired
        && v.pointer("/status/replicas").and_then(Value::as_i64) == Some(desired)
}
fn active(e: &Entry, v: &Value) -> bool {
    match v
        .pointer("/spec/template/metadata/annotations")
        .and_then(|v| v.get(MARK))
        .and_then(Value::as_str)
    {
        Some(mark) => mark == e.id,
        None => e.action == "Existing image",
    }
}
fn finalize(v: &Value, entries: &mut [Entry]) {
    for e in entries {
        if e.image == image(v)
            && active(e, v)
            && e.generation.is_some()
            && matches!(e.status.as_str(), "Deploying" | "Paused" | "Failed")
        {
            if ready(v) {
                e.status = "Running".into();
                e.message = "Rollout reached readiness.".into();
            } else if v.pointer("/spec/replicas").and_then(Value::as_i64) == Some(0) {
                e.status = "Paused".into();
                e.message = "Application is scaled to zero; readiness is not verified.".into();
            }
        }
    }
}
/// Called inside the workload update lock, before the resource-version patch.
pub fn enrich(current: &Value, patch: &mut Value) {
    let mut entries = read(current);
    if entries.is_empty() {
        let mut baseline = entry(current, "Existing image");
        baseline.message =
            "Recorded when history was enabled; earlier deployments are unavailable.".into();
        entries.push(baseline);
    }
    finalize(current, &mut entries);
    let proposed = image(patch);
    let changed = !proposed.is_empty() && proposed != image(current);
    let new_build = annotation(patch, "kubeweasel.io/build-job")
        .filter(|b| Some(*b) != annotation(current, "kubeweasel.io/build-job"));
    if let Some(build) = new_build {
        let mut e = entry(current, "Git build");
        e.build = Some(build.into());
        e.generation = None;
        e.status = "Building".into();
        e.message = "Building and publishing image.".into();
        if let Some(source) = annotation(patch, "kubeweasel.io/git-source")
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
        {
            e.image = format!(
                "{}:{build}",
                source["image_repository"].as_str().unwrap_or("unknown")
            );
        }
        entries.push(e);
    }
    let build = annotation(current, "kubeweasel.io/build-job");
    if changed
        || annotation(patch, "kubeweasel.io/rollback-from").is_some()
        || (annotation(patch, "kubeweasel.io/build-status") == Some("Deploying") && build.is_some())
    {
        let promoting = annotation(patch, "kubeweasel.io/build-status") == Some("Deploying");
        let index = if promoting {
            entries
                .iter()
                .position(|e| e.build.as_deref() == build && e.status == "Building")
        } else {
            None
        };
        let i = if let Some(i) = index {
            i
        } else {
            entries.push(entry(
                current,
                if annotation(patch, "kubeweasel.io/rollback-from").is_some() {
                    "Rollback"
                } else {
                    "Image update"
                },
            ));
            entries.len() - 1
        };
        let e = &mut entries[i];
        e.image = if proposed.is_empty() {
            image(current)
        } else {
            proposed
        };
        e.status = "Deploying".into();
        e.message = "Waiting for rollout readiness.".into();
        e.generation = Some(
            current
                .pointer("/metadata/generation")
                .and_then(Value::as_i64)
                .unwrap_or(0)
                + 1,
        );
        e.resolved_image = None;
        if patch
            .pointer("/spec/template/spec/imagePullSecrets")
            .is_some()
        {
            e.pull_secrets = secrets(patch);
        }
        patch["spec"]["template"]["metadata"]["annotations"][MARK] = json!(e.id);
    }
    if matches!(
        annotation(patch, "kubeweasel.io/build-status"),
        Some("Failed" | "Deployment failed")
    ) && let Some(e) = entries
        .iter_mut()
        .rev()
        .find(|e| e.build.as_deref() == build && e.status == "Building")
    {
        e.status = "Failed".into();
        e.message = annotation(patch, "kubeweasel.io/build-message")
            .unwrap_or("Build failed")
            .chars()
            .take(600)
            .collect();
    }
    if patch
        .pointer("/metadata/annotations")
        .and_then(|a| a.get("kubeweasel.io/build-job"))
        .is_some_and(Value::is_null)
    {
        for e in &mut entries {
            if e.status == "Building" {
                e.status = "Superseded".into();
                e.message =
                    "A manual image selection or rollback replaced this pending deployment.".into();
            }
        }
    }
    // Observation updates pass the versioned history directly; all other patches
    // enrich the latest history, avoiding lost records from concurrent settings edits.
    if patch
        .pointer("/metadata/annotations")
        .and_then(|v| v.get(KEY))
        .is_none()
    {
        if entries.len() > 30 {
            entries.drain(..entries.len() - 30);
        }
        patch["metadata"]["annotations"][KEY] = json!(serde_json::to_string(&entries).unwrap());
    }
}
fn pinned_image(original: &str, image_id: &str) -> Option<String> {
    let (_, digest) = image_id.split_once("@sha256:")?;
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let repository = original.split('@').next()?;
    let slash = repository.rfind('/').map(|i| i + 1).unwrap_or(0);
    let repository = if let Some(colon) = repository[slash..].rfind(':') {
        &repository[..slash + colon]
    } else {
        repository
    };
    Some(format!("{repository}@sha256:{digest}"))
}
async fn snapshot(s: &AppState, name: &str) -> Result<Vec<Entry>> {
    let w = managed(s, name).await?;
    let d = w.get(name).await?;
    let value = serde_json::to_value(&d).map_err(|_| invalid("Cannot read workload history"))?;
    let mut entries = read(&value);
    if entries.is_empty() {
        entries.push(entry(&value, "Existing image"));
    }
    finalize(&value, &mut entries);
    if let Some(client) = s.client.clone() {
        let pods: Api<Pod> = Api::namespaced(client, &s.namespace);
        let pods = pods
            .list(&ListParams::default().labels(&format!("{LABEL},app.kubernetes.io/name={name}")))
            .await?;
        for e in &mut entries {
            if e.image != image(&value) || !active(e, &value) || e.generation.is_none() {
                continue;
            }
            let matching: Vec<_> = pods
                .items
                .iter()
                .filter(|p| {
                    p.metadata.deletion_timestamp.is_none()
                        && p.spec.as_ref().is_some_and(|s| {
                            s.containers
                                .iter()
                                .any(|c| c.name == "app" && c.image.as_deref() == Some(&e.image))
                        })
                        && (p.annotations().get(MARK).map(String::as_str) == Some(&e.id)
                            || e.action == "Existing image")
                })
                .collect();
            if e.status == "Running" && e.resolved_image.is_none() {
                let digests: Vec<_> = matching
                    .iter()
                    .filter_map(|p| {
                        p.status
                            .as_ref()?
                            .container_statuses
                            .as_ref()?
                            .iter()
                            .find(|c| c.name == "app" && c.ready)
                    })
                    .filter_map(|c| pinned_image(&e.image, &c.image_id))
                    .collect();
                if let Some(first) = digests.first()
                    && digests.iter().all(|d| d == first)
                {
                    e.resolved_image = Some(first.clone());
                }
            } else if matches!(e.status.as_str(), "Deploying" | "Failed") {
                let failure = matching
                    .iter()
                    .filter_map(|p| p.status.as_ref())
                    .filter_map(|s| s.container_statuses.as_ref())
                    .flatten()
                    .filter_map(|c| c.state.as_ref()?.waiting.as_ref())
                    .find(|w| {
                        matches!(
                            w.reason.as_deref(),
                            Some(
                                "ImagePullBackOff"
                                    | "ErrImagePull"
                                    | "CrashLoopBackOff"
                                    | "CreateContainerConfigError"
                            )
                        )
                    });
                if let Some(failure) = failure {
                    e.status = "Failed".into();
                    e.message = failure.reason.clone().unwrap_or_default();
                }
                if value
                    .pointer("/status/conditions")
                    .and_then(Value::as_array)
                    .is_some_and(|cs| {
                        cs.iter()
                            .any(|c| c["type"] == "Progressing" && c["status"] == "False")
                    })
                {
                    e.status = "Failed".into();
                    e.message = "Kubernetes reported a rollout failure.".into();
                }
            }
        }
    }
    let encoded = serde_json::to_string(&entries).unwrap();
    if annotation(&value, KEY) != Some(encoded.as_str()) {
        w.patch(name,&PatchParams::default(),&Patch::Merge(json!({"metadata":{"resourceVersion":d.metadata.resource_version,"annotations":{KEY:encoded}}}))).await?;
    }
    Ok(entries)
}
async fn list(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    if s.client.is_none() {
        return Ok(Json(json!({"entries":[],"demo":true})));
    }
    let entries = snapshot(&s, &name).await?;
    let current = managed(&s, &name).await?.get(&name).await?;
    let current_image = image(&serde_json::to_value(current).unwrap());
    Ok(Json(
        json!({"entries":entries.into_iter().rev().map(|e|{let can=e.status=="Running" && e.image!=current_image;json!({"id":e.id,"time":e.time,"action":e.action,"image":e.image,"resolved_image":e.resolved_image,"status":e.status,"message":e.message,"can_rollback":can})}).collect::<Vec<_>>(),"demo":false}),
    ))
}
#[derive(Deserialize)]
struct Rollback {
    id: String,
}
async fn rollback(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Rollback>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = git::GATE.lock().await;
    let entry = snapshot(&s, &name)
        .await?
        .into_iter()
        .find(|e| e.id == input.id && e.status == "Running")
        .ok_or_else(|| invalid("Choose a deployment that previously reached readiness."))?;
    let w = managed(&s, &name).await?;
    let mut pulls = vec![json!({"$patch":"replace"})];
    pulls.extend(entry.pull_secrets.iter().map(|name| json!({"name":name})));
    w.patch(&name,&PatchParams::default(),&Patch::Strategic(json!({"metadata":{"annotations":{"kubeweasel.io/rollback-from":entry.id,"kubeweasel.io/git-source":null,"kubeweasel.io/build-job":null,"kubeweasel.io/build-status":null,"kubeweasel.io/build-message":null}},"spec":{"template":{"spec":{"imagePullSecrets":pulls,"containers":[{"name":"app","image":entry.resolved_image.unwrap_or(entry.image)}]}}}}))).await?;
    Ok(Json(json!({"ok":true})))
}
pub fn worker(s: AppState) {
    tokio::spawn(async move {
        loop {
            if let Some(client) = s.client.clone()
                && let Ok(apps) = workloads::list(client, &s.namespace, LABEL).await
            {
                for (_, app) in apps {
                    let _ = snapshot(&s, &app.name_any()).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    });
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/{name}/history", get(list))
        .route("/api/apps/{name}/rollback", post(rollback))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn old_ready_pods_do_not_complete_a_new_rollout() {
        let value = json!({"metadata":{"generation":2},"spec":{"replicas":1},"status":{"observedGeneration":2,"readyReplicas":1,"updatedReplicas":0,"replicas":2}});
        assert!(!ready(&value));
    }
    #[test]
    fn history_keeps_the_latest_thirty_releases() {
        let mut current = json!({"metadata":{"generation":1},"spec":{"template":{"spec":{"containers":[{"name":"app","image":"app:old"}]}}}});
        let entries: Vec<_> = (0..40).map(|_| entry(&current, "Image update")).collect();
        current["metadata"]["annotations"][KEY] = json!(serde_json::to_string(&entries).unwrap());
        let mut patch =
            json!({"spec":{"template":{"spec":{"containers":[{"name":"app","image":"app:new"}]}}}});
        enrich(&current, &mut patch);
        assert_eq!(read(&patch).len(), 30);
        assert_eq!(read(&patch).last().unwrap().image, "app:new");
    }
    #[test]
    fn first_git_promotion_is_recorded_when_the_image_reference_is_unchanged() {
        let mut current = json!({"metadata":{"generation":1},"spec":{"replicas":0,"template":{"spec":{"containers":[{"name":"app","image":"example/app:job"}]}}}});
        let mut build = json!({"metadata":{"annotations":{"kubeweasel.io/build-job":"job","kubeweasel.io/git-source":"{\"image_repository\":\"example/app\"}","kubeweasel.io/build-status":"Building"}}});
        enrich(&current, &mut build);
        current["metadata"]["annotations"] = build["metadata"]["annotations"].clone();
        let mut promotion = json!({"metadata":{"annotations":{"kubeweasel.io/build-status":"Deploying"}},"spec":{"replicas":1,"template":{"spec":{"containers":[{"name":"app","image":"example/app:job"}]}}}});
        enrich(&current, &mut promotion);
        assert_eq!(read(&promotion).last().unwrap().status, "Deploying");
        assert!(
            promotion
                .pointer("/spec/template/metadata/annotations")
                .unwrap()
                .get(MARK)
                .is_some()
        );
    }
    #[test]
    fn digest_preserves_registry_port() {
        let digest = "a".repeat(64);
        assert_eq!(
            pinned_image(
                "registry.example:5000/team/app:v2",
                &format!("docker-pullable://registry.example:5000/team/app@sha256:{digest}")
            ),
            Some(format!("registry.example:5000/team/app@sha256:{digest}"))
        );
        assert!(pinned_image("a:v2", "sha256:short").is_none());
    }
    #[test]
    fn history_is_bounded_and_contains_no_environment() {
        let current = json!({"metadata":{"generation":1},"spec":{"replicas":1,"template":{"spec":{"containers":[{"name":"app","image":"nginx:1","env":[{"name":"TOKEN","value":"secret-value"}]}]}}}});
        let mut p =
            json!({"spec":{"template":{"spec":{"containers":[{"name":"app","image":"nginx:2"}]}}}});
        enrich(&current, &mut p);
        let h = read(&p);
        assert_eq!(h.len(), 2);
        assert_eq!(h[1].image, "nginx:2");
        assert!(
            !p["metadata"]["annotations"][KEY]
                .as_str()
                .unwrap()
                .contains("secret-value")
        );
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Namespace, PersistentVolumeClaim};
    use kube::api::DeleteParams;
    async fn wait_status(
        s: &AppState,
        status: &str,
    ) -> std::result::Result<Vec<Entry>, Box<dyn std::error::Error>> {
        for _ in 0..120 {
            if let Ok(entries) = snapshot(s, "history-check").await
                && entries.last().is_some_and(|e| e.status == status)
            {
                return Ok(entries);
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        Err(format!("Timed out waiting for {status}").into())
    }
    #[tokio::test]
    #[ignore = "deploys a good/broken image and rolls back in an isolated namespace"]
    async fn rollback_recovers_image_and_preserves_storage_and_configuration() {
        assert_eq!(
            std::env::var("KUBEWEASEL_RUN_HISTORY_TEST").as_deref(),
            Ok("1")
        );
        let client = Client::try_default().await.unwrap();
        let ns = format!("kw-history-check-{}", id());
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let namespace: Namespace = serde_json::from_value(
            json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":ns}}),
        )
        .unwrap();
        namespaces
            .create(&PostParams::default(), &namespace)
            .await
            .unwrap();
        let result:std::result::Result<(),Box<dyn std::error::Error>>=async{
   let s=AppState{client:Some(client.clone()),namespace:ns.clone(),apps:Arc::new(RwLock::new(BTreeMap::new())),settings:settings::SettingsState::default(),storage:storage::StorageState::default(),projects:projects::Store::open(":memory:")?};let mut headers=HeaderMap::new();headers.insert("x-kubeweasel-request","dashboard".parse()?);
   let _=crate::deploy(State(s.clone()),headers.clone(),Json(Deploy{workload_kind:"Deployment".into(),project_id:None,name:"history-check".into(),image:"nginx:alpine".into(),port:80,replicas:1})).await.map_err(|e|e.1)?;
   let pvc:PersistentVolumeClaim=serde_json::from_value(json!({"apiVersion":"v1","kind":"PersistentVolumeClaim","metadata":{"name":"keep-data"},"spec":{"accessModes":["ReadWriteOnce"],"resources":{"requests":{"storage":"1Gi"}}}}))?;let claims:Api<PersistentVolumeClaim>=Api::namespaced(client.clone(),&ns);let original_claim=claims.create(&PostParams::default(),&pvc).await?;
   let w=managed(&s,"history-check").await.map_err(|e|e.1)?;
   w.patch("history-check",&PatchParams::default(),&Patch::Strategic(json!({"spec":{"strategy":{"type":"Recreate","rollingUpdate":null},"template":{"spec":{"terminationGracePeriodSeconds":1,"volumes":[{"name":"data","persistentVolumeClaim":{"claimName":"keep-data"}}],"containers":[{"name":"app","env":[{"name":"KEEP","value":"yes"}],"volumeMounts":[{"name":"data","mountPath":"/saved"}]}]}}}}))).await.map_err(|e|e.1)?;
   let good=wait_status(&s,"Running").await?;let target=good.last().unwrap().id.clone();
   w.patch("history-check",&PatchParams::default(),&Patch::Strategic(json!({"spec":{"template":{"spec":{"containers":[{"name":"app","image":"registry.invalid/kubeweasel/missing:bad"}]}}}}))).await.map_err(|e|e.1)?;
   wait_status(&s,"Failed").await?;
   let _=rollback(State(s.clone()),Path("history-check".into()),headers,Json(Rollback{id:target})).await.map_err(|e|e.1)?;
   let recovered=wait_status(&s,"Running").await?;assert_eq!(recovered.last().unwrap().action,"Rollback");
   let d=w.get("history-check").await.map_err(|e|e.1)?;let spec=d.spec.unwrap();assert_eq!(spec.replicas,Some(1));let pod=spec.template.spec.unwrap();assert_eq!(pod.containers[0].env.as_ref().unwrap()[0].value.as_deref(),Some("yes"));assert_eq!(pod.volumes.unwrap()[0].persistent_volume_claim.as_ref().unwrap().claim_name,"keep-data");assert_eq!(claims.get("keep-data").await?.metadata.uid,original_claim.metadata.uid);
   assert_eq!(recovered.iter().filter(|e|e.status=="Failed").count(),1);Ok(())
  }.await;
        namespaces
            .delete(&ns, &DeleteParams::default())
            .await
            .unwrap();
        result.unwrap();
    }
}
