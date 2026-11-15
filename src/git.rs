//! Public Git / Dockerfile builds. Jobs and workload annotations survive dashboard restarts.
use crate::{
    applications::{self, models::Deploy},
    error::{Error, Result, invalid},
    http::request::mutation,
    state::AppState,
    validation::{valid_name, validate},
    workloads::{self, LABEL, managed},
};
#[cfg(test)]
use crate::{projects, settings, storage};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use k8s_openapi::api::{
    batch::v1::Job,
    core::v1::{Pod, Secret},
};
#[cfg(test)]
use kube::Client;
use kube::{
    Api, ResourceExt,
    api::{ListParams, LogParams, Patch, PatchParams, PostParams},
};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
#[cfg(test)]
use std::{collections::BTreeMap, sync::Arc};
#[cfg(test)]
use tokio::sync::RwLock;

const SOURCE: &str = "kubeweasel.io/git-source";
const BUILD: &str = "kubeweasel.io/build-job";
const STATUS: &str = "kubeweasel.io/build-status";
const MESSAGE: &str = "kubeweasel.io/build-message";
const BUILD_LABEL: &str = "app.kubernetes.io/managed-by=kubeweasel-builder";

#[derive(Clone, Serialize, Deserialize)]
struct Source {
    repository: String,
    branch: String,
    dockerfile: String,
    registry: String,
    image_repository: String,
    #[serde(default)]
    credential_secret: String,
}
#[derive(Deserialize)]
struct Input {
    #[serde(flatten)]
    source: Source,
    #[serde(default)]
    username: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default = "default_port")]
    port: u16,
}
fn default_port() -> u16 {
    80
}
fn validate_source(s: &Source) -> Result<String> {
    let url = reqwest::Url::parse(&s.repository)
        .map_err(|_| invalid("Enter a public HTTPS Git repository URL."))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.port().is_some()
        || !matches!(
            url.host_str(),
            Some("github.com" | "gitlab.com" | "codeberg.org" | "bitbucket.org")
        )
        || s.repository.len() > 500
        || url.path().split('/').filter(|s| !s.is_empty()).count() < 2
    {
        return Err(invalid(
            "Use a public HTTPS repository on GitHub, GitLab, Codeberg, or Bitbucket, without credentials or URL parameters.",
        ));
    }
    if s.branch.is_empty()
        || s.branch.len() > 200
        || s.branch.starts_with('-')
        || s.branch.contains("..")
        || !s
            .branch
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
    {
        return Err(invalid(
            "Enter a branch name using letters, numbers, /, _, -, or single dots.",
        ));
    }
    if s.dockerfile.len() > 250
        || s.dockerfile
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        || !s
            .dockerfile
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b))
    {
        return Err(invalid(
            "Use a Dockerfile path relative to the repository root, without . or .. segments.",
        ));
    }
    let image = &s.image_repository;
    if image.len() > 250
        || image
            .bytes()
            .any(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b"/._-:".contains(&b)))
        || image.contains("..")
        || image.contains("//")
        || image.ends_with('/')
        || image.split('/').count() < 2
        || (s.registry != "custom" && image.split('/').count() < 3)
        || image.rsplit('/').next().unwrap_or("").contains(':')
    {
        return Err(invalid(
            "Enter a lowercase image repository including registry and owner, without a tag (for example docker.io/your-name/your-app).",
        ));
    }
    let host = image.split('/').next().unwrap_or("");
    match s.registry.as_str() {
        "dockerhub" if host == "docker.io" => {}
        "github" if host == "ghcr.io" => {}
        "custom" if host.contains('.') || host.contains(':') => {}
        _ => {
            return Err(invalid(
                "The image repository must match the selected registry: docker.io, ghcr.io, or a custom registry hostname.",
            ));
        }
    }
    let registry_url = reqwest::Url::parse(&format!("https://{host}"))
        .map_err(|_| invalid("Invalid registry hostname."))?;
    if registry_url.host_str().is_none()
        || !registry_url.username().is_empty()
        || registry_url.password().is_some()
    {
        return Err(invalid("Invalid registry hostname."));
    }
    Ok(host.into())
}
fn id() -> String {
    let mut bytes = [0u8; 10];
    rand_core::OsRng.fill_bytes(&mut bytes);
    format!(
        "kw-build-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn owned_job(job: &Job, name: &str) -> Result<()> {
    if job
        .labels()
        .get("app.kubernetes.io/managed-by")
        .map(String::as_str)
        != Some("kubeweasel-builder")
        || job
            .labels()
            .get("kubeweasel.io/build-app")
            .map(String::as_str)
            != Some(name)
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "This build is not managed by KubeWeasel for this app.".into(),
        ));
    }
    Ok(())
}
fn job_body(ns: &str, name: &str, build: &str, source: &Source, image: &str) -> Value {
    let mut pod = json!({"automountServiceAccountToken":false,"restartPolicy":"Never","terminationGracePeriodSeconds":10,
      "securityContext":{"runAsUser":1000,"runAsGroup":1000,"fsGroup":1000},
      "containers":[{"name":"build","image":"moby/buildkit:v0.33.0-rootless","command":["buildctl-daemonless.sh"],
        "args":["build","--progress=plain","--frontend=dockerfile.v0","--opt",format!("context={}#{}",source.repository,source.branch),"--opt",format!("filename={}",source.dockerfile),"--output",format!("type=image,name={image},push=true")],
        "env":[{"name":"BUILDKITD_FLAGS","value":"--oci-worker-no-process-sandbox --oci-worker-snapshotter=native"},{"name":"DOCKER_CONFIG","value":"/registry"}],
        "securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"Unconfined"},"appArmorProfile":{"type":"Unconfined"}},
        "resources":{"requests":{"cpu":"250m","memory":"256Mi","ephemeral-storage":"1Gi"},"limits":{"cpu":"2","memory":"2Gi","ephemeral-storage":"10Gi"}},
        "volumeMounts":[{"name":"build-cache","mountPath":"/home/user/.local/share/buildkit"}]}],
      "volumes":[{"name":"build-cache","emptyDir":{"sizeLimit":"10Gi"}}]});
    if !source.credential_secret.is_empty() {
        pod["volumes"].as_array_mut().unwrap().push(json!({"name":"registry","secret":{"secretName":source.credential_secret,"items":[{"key":".dockerconfigjson","path":"config.json"}]}}));
        pod["containers"][0]["volumeMounts"]
            .as_array_mut()
            .unwrap()
            .push(json!({"name":"registry","mountPath":"/registry","readOnly":true}));
    }
    json!({"apiVersion":"batch/v1","kind":"Job","metadata":{"name":build,"namespace":ns,"labels":{"app.kubernetes.io/managed-by":"kubeweasel-builder","kubeweasel.io/build-app":name},"annotations":{"kubeweasel.io/build-image":image}},
      "spec":{"backoffLimit":0,"activeDeadlineSeconds":1800,"ttlSecondsAfterFinished":604800,"template":{"metadata":{"labels":{"app.kubernetes.io/managed-by":"kubeweasel-builder","kubeweasel.io/build-app":name}},"spec":pod}}})
}
async fn annotate(workload: &workloads::Managed, name: &str, annotations: Value) -> Result<()> {
    workload
        .patch(
            name,
            &PatchParams::default(),
            &Patch::Merge(json!({"metadata":{"annotations":annotations}})),
        )
        .await?;
    Ok(())
}
async fn start(
    s: &AppState,
    name: &str,
    mut input: Input,
    create: bool,
    headers: HeaderMap,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.build_gate.lock().await;
    let host = validate_source(&input.source)?;
    let client = s.client.clone().ok_or_else(|| {
        invalid("Git builds require a live Kubernetes cluster; demo mode does not build images.")
    })?;
    if !valid_name(name) {
        return Err(invalid("Choose a valid application name."));
    }
    let jobs: Api<Job> = Api::namespaced(client.clone(), &s.namespace);
    let active = jobs
        .list(&ListParams::default().labels(BUILD_LABEL))
        .await?
        .items
        .into_iter()
        .filter(|j| !finished(j))
        .count();
    if active >= 2 {
        return Err(invalid(
            "Two builds are already running. Wait for one to finish.",
        ));
    }
    let mut source = input.source.clone();
    // Never trust a caller-supplied Secret reference. Reuse only the app's saved reference.
    source.credential_secret.clear();
    if !create {
        let w = managed(s, name).await?;
        let current = w.get(name).await?;
        if current
            .annotations()
            .get(STATUS)
            .is_some_and(|v| v == "Building")
        {
            return Err(invalid("This application already has a build in progress."));
        }
        if let Some(saved) = current
            .annotations()
            .get(SOURCE)
            .and_then(|v| serde_json::from_str::<Source>(v).ok())
            && saved.registry == source.registry
            && saved.image_repository == source.image_repository
        {
            source.credential_secret = saved.credential_secret;
        }
    } else {
        if input.project_id.is_none() {
            return Err(invalid("Choose a project for this application."));
        }
        s.projects
            .require(&s.namespace, input.project_id.as_deref())?;
        workloads::name_available(client.clone(), &s.namespace, name).await?;
        validate(&Deploy {
            workload_kind: "Deployment".into(),
            project_id: input.project_id.clone(),
            name: name.into(),
            image: format!("{}:pending", source.image_repository),
            port: input.port,
            replicas: 0,
        })?;
    }
    if !input.token.is_empty() {
        if input.token.len() > 4096 || input.username.is_empty() || input.username.len() > 200 {
            return Err(invalid(
                "Provide the registry username and a token of at most 4096 characters.",
            ));
        }
        source.credential_secret = format!("{}-registry", id());
        let auth = STANDARD.encode(format!("{}:{}", input.username, input.token));
        let config = json!({"auths":{if host=="docker.io"{"https://index.docker.io/v1/"}else{&host}: {"auth":auth}}});
        let secret:Secret=serde_json::from_value(json!({"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/dockerconfigjson","metadata":{"name":source.credential_secret,"labels":{"app.kubernetes.io/managed-by":"kubeweasel-builder"}},"stringData":{".dockerconfigjson":config.to_string()}})).map_err(|_|invalid("Cannot prepare registry credentials."))?;
        // Do not include Kubernetes response bodies in credential errors.
        Api::<Secret>::namespaced(client.clone(),&s.namespace).create(&PostParams::default(),&secret).await.map_err(|_|Error(StatusCode::BAD_GATEWAY,"Could not save registry credentials. Check permission to create Secrets in this namespace.".into()))?;
        input.token.clear();
    }
    if source.credential_secret.is_empty() {
        return Err(invalid(
            "Enter your registry username and access token to publish images.",
        ));
    }
    let build = id();
    let image = format!("{}:{build}", source.image_repository);
    if create {
        validate(&Deploy {
            workload_kind: "Deployment".into(),
            project_id: input.project_id.clone(),
            name: name.into(),
            image: image.clone(),
            port: input.port,
            replicas: 0,
        })?;
        applications::service::deploy(
            s,
            Deploy {
                workload_kind: "Deployment".into(),
                project_id: input.project_id,
                name: name.into(),
                image: image.clone(),
                port: input.port,
                replicas: 0,
            },
        )
        .await?;
    }
    let workload = managed(s, name).await?;
    annotate(&workload,name,json!({SOURCE:serde_json::to_string(&source).map_err(|_|invalid("Invalid Git source."))?,BUILD:build,STATUS:"Building",MESSAGE:"Building and publishing the image.","kubeweasel.io/build-initial":if create{"true"}else{"false"}})).await?;
    let body: Job = serde_json::from_value(job_body(&s.namespace, name, &build, &source, &image))
        .map_err(|_| invalid("Invalid build Job."))?;
    if let Err(error) = jobs.create(&PostParams::default(), &body).await {
        let _=annotate(&workload,name,json!({STATUS:"Failed",MESSAGE:"Build Job could not be created. Check cluster permissions and rebuild."})).await;
        return Err(Error(
            StatusCode::BAD_GATEWAY,
            format!(
                "Application source saved, but build could not start: {error}. Open the app's Source tab to retry."
            ),
        ));
    }
    Ok(Json(json!({"ok":true,"name":name,"build":build})))
}
fn finished(job: &Job) -> bool {
    job.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| {
            cs.iter()
                .any(|c| (c.type_ == "Complete" || c.type_ == "Failed") && c.status == "True")
        })
}
async fn create(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    let name = input.name.clone();
    start(&s, &name, input, true, headers).await
}
async fn rebuild(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    start(&s, &name, input, false, headers).await
}
async fn inspect(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    if s.client.is_none() {
        return Ok(Json(
            json!({"source":null,"status":"Unavailable in demo mode","logs":""}),
        ));
    }
    let w = managed(&s, &name).await?;
    let d = w.get(&name).await?;
    let source = d
        .annotations()
        .get(SOURCE)
        .and_then(|v| serde_json::from_str::<Source>(v).ok());
    let saved_status = d.annotations().get(STATUS).map(String::as_str);
    let ready = d.status.as_ref().is_some_and(|status| {
        status.observed_generation == d.metadata.generation
            && status.ready_replicas.unwrap_or(0)
                >= d.spec.as_ref().and_then(|v| v.replicas).unwrap_or(1)
    });
    let status = if saved_status == Some("Deploying") && ready {
        Some(if d.spec.as_ref().and_then(|v| v.replicas) == Some(0) {
            "Paused"
        } else {
            "Running"
        })
    } else {
        saved_status
    };
    let mut result = json!({"source":source.as_ref().map(|v|json!({"repository":v.repository,"branch":v.branch,"dockerfile":v.dockerfile,"registry":v.registry,"image_repository":v.image_repository,"has_credentials":!v.credential_secret.is_empty()})),"status":status,"message":d.annotations().get(MESSAGE),"logs":"","build":d.annotations().get(BUILD)});
    if let Some(build) = d.annotations().get(BUILD) {
        let jobs: Api<Job> = Api::namespaced(s.client.clone().unwrap(), &s.namespace);
        if let Some(job) = jobs.get_opt(build).await? {
            owned_job(&job, &name)?;
            let pods: Api<Pod> = Api::namespaced(s.client.clone().unwrap(), &s.namespace);
            let list = pods
                .list(&ListParams::default().labels(&format!("job-name={build},{BUILD_LABEL}")))
                .await?;
            if let Some(p) = list.items.first() {
                let logs = pods
                    .logs(
                        &p.name_any(),
                        &LogParams {
                            container: Some("build".into()),
                            tail_lines: Some(250),
                            limit_bytes: Some(65536),
                            ..Default::default()
                        },
                    )
                    .await;
                result["logs"] = json!(logs.unwrap_or_else(|_| {
                    "Build logs will appear when the builder starts.".into()
                }));
                if let Some(status) = &p.status {
                    result["pod_phase"] = json!(status.phase);
                    result["pod_message"] = json!(
                        status
                            .container_statuses
                            .as_ref()
                            .and_then(|cs| cs.first())
                            .and_then(|c| c.state.as_ref())
                            .and_then(|s| s.waiting.as_ref())
                            .and_then(|w| w.message.clone())
                    );
                }
                if result["pod_message"].is_null() {
                    result["pod_message"] = json!(
                        p.status
                            .as_ref()
                            .and_then(|s| s.conditions.as_ref())
                            .and_then(|cs| cs
                                .iter()
                                .find(|c| c.type_ == "PodScheduled" && c.status == "False"))
                            .and_then(|c| c.message.clone())
                    );
                }
            }
        } else {
            result["logs"] = json!("Build logs have expired or the build Job was removed.");
        }
    }
    Ok(Json(result))
}
async fn reconcile(s: &AppState) -> Result<()> {
    let _gate = s.build_gate.lock().await;
    let Some(client) = s.client.clone() else {
        return Ok(());
    };
    let jobs: Api<Job> = Api::namespaced(client.clone(), &s.namespace);
    for (_, d) in workloads::list(client.clone(), &s.namespace, LABEL).await? {
        if d.annotations().get(STATUS).map(String::as_str) != Some("Building") {
            continue;
        }
        let Some(build) = d.annotations().get(BUILD) else {
            continue;
        };
        let name = d.name_any();
        let w = managed(s, &name).await?;
        let Some(job) = jobs.get_opt(build).await? else {
            annotate(
                &w,
                &name,
                json!({STATUS:"Failed",MESSAGE:"Build Job is missing. Rebuild to retry."}),
            )
            .await?;
            continue;
        };
        owned_job(&job, &name)?;
        if !finished(&job) {
            continue;
        }
        let success = job
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .is_some_and(|cs| {
                cs.iter()
                    .any(|c| c.type_ == "Complete" && c.status == "True")
            });
        if !success {
            annotate(&w,&name,json!({STATUS:"Failed",MESSAGE:"Build failed. The running image was not changed. Check build logs and rebuild."})).await?;
            continue;
        }
        let source: Source = serde_json::from_str(
            d.annotations()
                .get(SOURCE)
                .ok_or_else(|| invalid("Missing source"))?,
        )
        .map_err(|_| invalid("Invalid saved source"))?;
        let image = job
            .annotations()
            .get("kubeweasel.io/build-image")
            .ok_or_else(|| invalid("Missing build image"))?;
        // Image and status are committed together. Retrying after a dashboard restart is safe.
        let replicas = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
        let first = d
            .annotations()
            .get("kubeweasel.io/build-initial")
            .map(String::as_str)
            == Some("true");
        if let Err(error) = w.patch(&name,&PatchParams::default(),&Patch::Strategic(json!({"metadata":{"annotations":{STATUS:"Deploying",MESSAGE:"Image published and deployment requested. Check pod readiness in the project.","kubeweasel.io/git-deployed":"true"}},"spec":{"replicas":if first{1}else{replicas},"template":{"spec":{"imagePullSecrets":[{"name":source.credential_secret}],"containers":[{"name":"app","image":image}]}}}}))).await {
            annotate(&w,&name,json!({STATUS:"Deployment failed",MESSAGE:format!("The build succeeded, but the workload could not be updated: {}. Check the app configuration and rebuild.",error.1)})).await?;
        }
    }
    Ok(())
}
pub fn worker(s: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            if let Err(e) = reconcile(&s).await {
                eprintln!("Git build reconciliation: {}", e.1);
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    })
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/git", post(create))
        .route("/api/apps/{name}/source", get(inspect).post(rebuild))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn source() -> Source {
        Source {
            repository: "https://github.com/docker/welcome-to-docker.git".into(),
            branch: "main".into(),
            dockerfile: "Dockerfile".into(),
            registry: "dockerhub".into(),
            image_repository: "docker.io/example/web".into(),
            credential_secret: String::new(),
        }
    }
    #[test]
    fn validates_source_without_arg_injection() {
        let mut s = source();
        assert!(validate_source(&s).is_ok());
        for bad in [
            "https://user:token@github.com/a/b",
            "http://github.com/a/b",
            "https://localhost/a/b",
            "https://github.com/a/b#main",
        ] {
            s.repository = bad.into();
            assert!(validate_source(&s).is_err());
        }
        s = source();
        for bad in ["main --flag", "../main", "main#evil"] {
            s.branch = bad.into();
            assert!(validate_source(&s).is_err());
        }
        s = source();
        s.image_repository = "docker.io/example/web,push=false".into();
        assert!(validate_source(&s).is_err());
        s = source();
        s.registry = "github".into();
        assert!(validate_source(&s).is_err());
        s.image_repository = "ghcr.io/example/web".into();
        assert!(validate_source(&s).is_ok());
        s.registry = "custom".into();
        s.image_repository = "registry.example.com:5000/team/web".into();
        assert!(validate_source(&s).is_ok());
        s.image_repository = "registry.example.com/web".into();
        assert!(validate_source(&s).is_ok());
    }
    #[test]
    fn build_has_no_cluster_token_and_no_user_shell() {
        let s = source();
        let j = job_body(
            "test",
            "app",
            "kw-build-test",
            &s,
            "docker.io/example/app:unique",
        );
        assert_eq!(j["spec"]["backoffLimit"], 0);
        let pod = &j["spec"]["template"]["spec"];
        assert_eq!(pod["automountServiceAccountToken"], false);
        assert_eq!(
            pod["containers"][0]["command"],
            json!(["buildctl-daemonless.sh"])
        );
        assert!(pod.get("hostPath").is_none());
        assert_eq!(pod["securityContext"]["runAsUser"], 1000);
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::DeleteParams;
    async fn wait_job(
        jobs: &Api<Job>,
        name: &str,
    ) -> std::result::Result<Job, Box<dyn std::error::Error>> {
        for _ in 0..240 {
            let j = jobs.get(name).await?;
            if finished(&j) {
                return Ok(j);
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        Err("Build Job timed out".into())
    }
    #[tokio::test]
    #[ignore = "builds a public sample into an OCI archive in an isolated namespace; does not publish an image"]
    async fn public_git_build_and_rollout_recovery() {
        assert_eq!(std::env::var("KUBEWEASEL_RUN_GIT_TEST").as_deref(), Ok("1"));
        let client = Client::try_default().await.unwrap();
        let ns = format!("kw-git-check-{}", id().trim_start_matches("kw-build-"));
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let n: Namespace = serde_json::from_value(
            json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":ns}}),
        )
        .unwrap();
        namespaces.create(&PostParams::default(), &n).await.unwrap();
        let result:std::result::Result<(),Box<dyn std::error::Error>>=async {
            let s=AppState{build_gate:Default::default(),client:Some(client.clone()),namespace:ns.clone(),apps:Arc::new(RwLock::new(BTreeMap::new())),settings:settings::SettingsState::default(),storage:storage::StorageState::default(),projects:projects::Store::open(":memory:")?};
            let mut headers=HeaderMap::new();headers.insert("x-kubeweasel-request","dashboard".parse()?);
            applications::service::deploy(&s, Deploy{workload_kind:"Deployment".into(),project_id:None,name:"git-check".into(),image:"busybox:1.36".into(),port:8080,replicas:0}).await.map_err(|e|e.1)?;
            let workload=managed(&s,"git-check").await.map_err(|e|e.1)?;
            workload.patch("git-check",&PatchParams::default(),&Patch::Strategic(json!({"spec":{"template":{"spec":{"terminationGracePeriodSeconds":1,"containers":[{"name":"app","command":["sh","-c","sleep 3600"]}]}}}}))).await.map_err(|e|e.1)?;
            let source=Source{repository:"https://github.com/docker/welcome-to-docker.git".into(),branch:"main".into(),dockerfile:"Dockerfile".into(),registry:"dockerhub".into(),image_repository:"docker.io/library/busybox".into(),credential_secret:"test-registry".into()};
            let secret:Secret=serde_json::from_value(json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":"test-registry"},"type":"kubernetes.io/dockerconfigjson","stringData":{".dockerconfigjson":"{\"auths\":{}}"}}))?;
            Api::<Secret>::namespaced(client.clone(),&ns).create(&PostParams::default(),&secret).await?;
            let build=id();
            let mut body=job_body(&ns,"git-check",&build,&source,"busybox:1.37");
            // Exercise real Git checkout and Dockerfile execution without publishing
            // to someone else's registry or needing the owner's registry token.
            let args=body["spec"]["template"]["spec"]["containers"][0]["args"].as_array_mut().unwrap();
            *args.last_mut().unwrap()=json!("type=oci,dest=/tmp/sample.oci.tar");
            let job:Job=serde_json::from_value(body)?;
            let jobs:Api<Job>=Api::namespaced(client.clone(),&ns);
            jobs.create(&PostParams::default(),&job).await?;
            annotate(&workload,"git-check",json!({SOURCE:serde_json::to_string(&source)?,BUILD:build,STATUS:"Building","kubeweasel.io/build-initial":"true"})).await.map_err(|e|e.1)?;
            let job=wait_job(&jobs,&build).await?;
            let pods:Api<Pod>=Api::namespaced(client.clone(),&ns);
            if job.status.as_ref().and_then(|v|v.succeeded).unwrap_or(0)!=1 {
                let list=pods.list(&ListParams::default().labels(&format!("job-name={build}"))).await?;
                for pod in list.items {eprintln!("Builder status: {:?}",pod.status);eprintln!("{}",pods.logs(&pod.name_any(),&LogParams::default()).await.unwrap_or_default());}
                return Err("Real public Git Dockerfile build failed".into());
            }
            reconcile(&s).await.map_err(|e|e.1)?;
            let deployed=workload.get("git-check").await.map_err(|e|e.1)?;
            assert_eq!(deployed.annotations().get(STATUS).map(String::as_str),Some("Deploying"));
            assert_eq!(deployed.spec.as_ref().unwrap().replicas,Some(1));
            assert_eq!(deployed.spec.as_ref().unwrap().template.spec.as_ref().unwrap().containers[0].image.as_deref(),Some("busybox:1.37"));
            let failure=id();let mut body=job_body(&ns,"git-check",&failure,&source,"busybox:broken");
            let c=&mut body["spec"]["template"]["spec"]["containers"][0];c["image"]=json!("busybox:1.37");c["command"]=json!(["sh","-c","echo expected-build-failure; exit 1"]);c["args"]=json!([]);c["securityContext"]=json!({"runAsUser":1000});
            let failed:Job=serde_json::from_value(body)?;jobs.create(&PostParams::default(),&failed).await?;
            annotate(&workload,"git-check",json!({BUILD:failure,STATUS:"Building","kubeweasel.io/build-initial":"false"})).await.map_err(|e|e.1)?;
            wait_job(&jobs,&failure).await?;reconcile(&s).await.map_err(|e|e.1)?;
            let after=workload.get("git-check").await.map_err(|e|e.1)?;
            assert_eq!(after.annotations().get(STATUS).map(String::as_str),Some("Failed"));
            assert_eq!(after.spec.as_ref().unwrap().template.spec.as_ref().unwrap().containers[0].image.as_deref(),Some("busybox:1.37"));
            let status=inspect(State(s),Path("git-check".into())).await.map_err(|e|e.1)?.0;
            assert!(status["source"].get("credential_secret").is_none());assert!(status["logs"].as_str().unwrap().contains("expected-build-failure"));
            Ok(())
        }.await;
        namespaces
            .delete(&ns, &DeleteParams::default())
            .await
            .unwrap();
        result.unwrap();
    }
}
