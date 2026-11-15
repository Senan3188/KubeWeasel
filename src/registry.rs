use crate::{
    applications::{self, models::Deploy},
    error::{Error, Result, invalid},
    http::request::mutation,
    state::AppState,
    validation::validate,
    workloads::{self, managed},
};
#[cfg(test)]
use crate::{projects, settings, storage};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use k8s_openapi::api::core::v1::Secret;
use kube::{
    Api,
    api::{Patch, PatchParams, PostParams},
};
#[cfg(test)]
use kube::{Client, ResourceExt};
use rand_core::{OsRng, RngCore};
use serde::Deserialize;
use serde_json::{Value, json};
#[cfg(test)]
use std::{collections::BTreeMap, sync::Arc};
#[cfg(test)]
use tokio::sync::RwLock;

#[derive(Deserialize)]
struct ImageInput {
    #[serde(flatten)]
    deploy: Deploy,
    registry: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    token: String,
}
fn image_reference(provider: &str, image: &str) -> Result<(String, String)> {
    if image.is_empty() || image.len() > 512 || image.chars().any(char::is_whitespace) {
        return Err(invalid(
            "Enter a container image, including its tag or digest.",
        ));
    }
    let explicit = image
        .split_once('/')
        .filter(|(host, _)| host.contains('.') || host.contains(':') || *host == "localhost");
    let expected = match provider {
        "dockerhub" => Some("docker.io"),
        "github" => Some("ghcr.io"),
        "custom" => None,
        _ => {
            return Err(invalid(
                "Choose Docker Hub, GitHub Container Registry, or a custom registry.",
            ));
        }
    };
    let reference = if let Some(host) = expected {
        if let Some((actual, _)) = explicit {
            if actual != host {
                return Err(invalid(
                    "The image hostname does not match the selected registry.",
                ));
            }
            image.to_string()
        } else if provider == "dockerhub" && !image.contains('/') {
            format!("docker.io/library/{image}")
        } else {
            format!("{host}/{image}")
        }
    } else {
        if explicit.is_none() {
            return Err(invalid(
                "For a custom registry, include its hostname: registry.example.com/team/app:tag.",
            ));
        }
        image.to_string()
    };
    let (host, path) = reference.split_once('/').unwrap();
    let url = reqwest::Url::parse(&format!("https://{host}"))
        .map_err(|_| invalid("Invalid registry hostname."))?;
    if url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || path.is_empty()
        || path.starts_with('/')
        || reference.contains([',', '\\', '?', '#'])
    {
        return Err(invalid(
            "Enter a valid image reference without a URL scheme or parameters.",
        ));
    }
    if provider == "github" && !path.contains('/') {
        return Err(invalid(
            "GitHub images need an owner and image name: ghcr.io/owner/app:tag.",
        ));
    }
    Ok((reference.clone(), host.into()))
}
async fn create(
    State(s): State<AppState>,
    headers: HeaderMap,
    Json(mut input): Json<ImageInput>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let (image, host) = image_reference(&input.registry, &input.deploy.image)?;
    input.deploy.image = image;
    validate(&input.deploy)?;
    s.projects
        .require(&s.namespace, input.deploy.project_id.as_deref())?;
    if input.username.is_empty() != input.token.is_empty()
        || input.username.len() > 200
        || input.token.len() > 4096
    {
        return Err(invalid(
            "Provide both registry username and token, or leave both empty for a public image.",
        ));
    }
    if s.client.is_none() && !input.token.is_empty() {
        return Err(invalid(
            "Demo mode cannot save registry credentials. Leave credentials empty to simulate a public image.",
        ));
    }
    let mut secret = None;
    if let Some(client) = s.client.clone() {
        workloads::name_available(client.clone(), &s.namespace, &input.deploy.name).await?;
        if !input.token.is_empty() {
            let mut random = [0u8; 10];
            OsRng.fill_bytes(&mut random);
            let name = format!(
                "kw-pull-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let host = if host == "docker.io" {
                "https://index.docker.io/v1/"
            } else {
                &host
            };
            let auth = STANDARD.encode(format!("{}:{}", input.username, input.token));
            let config = json!({"auths":{host:{"auth":auth}}});
            let body:Secret=serde_json::from_value(json!({"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/dockerconfigjson","metadata":{"name":name,"labels":{"app.kubernetes.io/managed-by":"kubeweasel"}},"stringData":{".dockerconfigjson":config.to_string()}})).map_err(|_|invalid("Cannot prepare registry credentials."))?;
            Api::<Secret>::namespaced(client,&s.namespace).create(&PostParams::default(),&body).await.map_err(|_|Error(StatusCode::BAD_GATEWAY,"Could not save image-pull credentials. Check permission to create Secrets in the app namespace.".into()))?;
            secret = Some(name);
        }
    }
    let credential = secret.clone();
    let result = applications::service::deploy_with_pull_secret(&s, input.deploy, secret).await;
    result.map_err(|e|if credential.is_some(){Error(e.0,format!("{}. Image-pull credentials were saved as a retained Kubernetes Secret; inspect the application before retrying.",e.1))}else{e})?;
    Ok(Json(json!({"ok":true})))
}
#[derive(Deserialize)]
struct UpdateInput {
    image: String,
    registry: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    token: String,
}
async fn update(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(input): Json<UpdateInput>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let _gate = s.build_gate.lock().await;
    let (image, host) = image_reference(&input.registry, &input.image)?;
    if input.username.is_empty() != input.token.is_empty()
        || input.username.len() > 200
        || input.token.len() > 4096
    {
        return Err(invalid(
            "Provide both registry username and token, or leave both empty to keep existing pull credentials.",
        ));
    }
    if let Some(client) = s.client.clone() {
        let workload = managed(&s, &name).await?;
        workload.get(&name).await?;
        let mut pod = json!({"containers":[{"name":"app","image":image}]});
        if !input.token.is_empty() {
            let mut random = [0u8; 10];
            OsRng.fill_bytes(&mut random);
            let secret_name = format!(
                "kw-pull-{}",
                random
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            );
            let host = if host == "docker.io" {
                "https://index.docker.io/v1/"
            } else {
                &host
            };
            let config = json!({"auths":{host:{"auth":STANDARD.encode(format!("{}:{}",input.username,input.token))}}});
            let secret:Secret=serde_json::from_value(json!({"apiVersion":"v1","kind":"Secret","type":"kubernetes.io/dockerconfigjson","metadata":{"name":secret_name,"labels":{"app.kubernetes.io/managed-by":"kubeweasel"}},"stringData":{".dockerconfigjson":config.to_string()}})).map_err(|_|invalid("Cannot prepare registry credentials."))?;
            Api::<Secret>::namespaced(client, &s.namespace)
                .create(&PostParams::default(), &secret)
                .await
                .map_err(|_| {
                    Error(
                        StatusCode::BAD_GATEWAY,
                        "Could not save registry credentials.".into(),
                    )
                })?;
            pod["imagePullSecrets"] = json!([{"name":secret_name}]);
        }
        // Clearing the pending build ID and source under the build gate prevents
        // an older Git build from replacing the explicitly selected image later.
        workload.patch(&name,&PatchParams::default(),&Patch::Strategic(json!({"metadata":{"annotations":{"kubeweasel.io/git-source":null,"kubeweasel.io/build-job":null,"kubeweasel.io/build-status":null,"kubeweasel.io/build-message":null,"kubeweasel.io/build-initial":null,"kubeweasel.io/git-deployed":null}},"spec":{"template":{"spec":pod}}}))).await?;
    } else {
        if !input.token.is_empty() {
            return Err(invalid("Demo mode cannot save registry credentials."));
        }
        let mut apps = s.apps.write().await;
        let app = apps
            .get_mut(&name)
            .ok_or_else(|| invalid("Application not found."))?;
        app.image = image;
    }
    Ok(Json(json!({"ok":true})))
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/image", post(create))
        .route("/api/apps/{name}/image", post(update))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn provider_references() {
        assert_eq!(
            image_reference("dockerhub", "nginx:stable").unwrap().0,
            "docker.io/library/nginx:stable"
        );
        assert_eq!(
            image_reference("github", "owner/app:latest").unwrap().0,
            "ghcr.io/owner/app:latest"
        );
        assert!(image_reference("dockerhub", "ghcr.io/owner/app").is_err());
        assert!(image_reference("custom", "owner/app").is_err());
        assert!(image_reference("custom", "registry.example.com:5000/app:tag").is_ok());
        assert!(image_reference("custom", "https://registry.example.com/app").is_err());
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::DeleteParams;
    #[tokio::test]
    #[ignore = "creates a paused workload in an isolated namespace to verify source switching"]
    async fn image_switch_clears_pending_build_without_changing_runtime_settings() {
        assert_eq!(
            std::env::var("KUBEWEASEL_RUN_SOURCE_TEST").as_deref(),
            Ok("1")
        );
        let client = Client::try_default().await.unwrap();
        let mut bytes = [0u8; 8];
        OsRng.fill_bytes(&mut bytes);
        let ns = format!(
            "kw-source-check-{}",
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
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
        let result:std::result::Result<(),Box<dyn std::error::Error>>=async {
            let s=AppState{build_gate:Default::default(),client:Some(client),namespace:ns.clone(),apps:Arc::new(RwLock::new(BTreeMap::new())),settings:settings::SettingsState::default(),storage:storage::StorageState::default(),projects:projects::Store::open(":memory:")?};
            let mut headers=HeaderMap::new();headers.insert("x-kubeweasel-request","dashboard".parse()?);
            applications::service::deploy(&s, Deploy{workload_kind:"Deployment".into(),project_id:None,name:"source-check".into(),image:"nginx:stable".into(),port:8080,replicas:0}).await.map_err(|e|e.1)?;
            let w=managed(&s,"source-check").await.map_err(|e|e.1)?;
            w.patch("source-check",&PatchParams::default(),&Patch::Strategic(json!({"metadata":{"annotations":{"kubeweasel.io/git-source":"{}","kubeweasel.io/build-job":"old-build","kubeweasel.io/build-status":"Building"}},"spec":{"template":{"spec":{"volumes":[{"name":"keep","emptyDir":{}}],"containers":[{"name":"app","env":[{"name":"KEEP","value":"yes"}],"volumeMounts":[{"name":"keep","mountPath":"/keep"}]}]}}}}))).await.map_err(|e|e.1)?;
            let _=update(State(s),Path("source-check".into()),headers,Json(UpdateInput{image:"library/nginx:alpine".into(),registry:"dockerhub".into(),username:String::new(),token:String::new()})).await.map_err(|e|e.1)?;
            let d=w.get("source-check").await.map_err(|e|e.1)?;
            assert!(!d.annotations().contains_key("kubeweasel.io/build-job"));assert!(!d.annotations().contains_key("kubeweasel.io/git-source"));
            let spec=d.spec.unwrap();assert_eq!(spec.replicas,Some(0));let pod=spec.template.spec.unwrap();assert_eq!(pod.containers[0].image.as_deref(),Some("docker.io/library/nginx:alpine"));assert_eq!(pod.containers[0].env.as_ref().unwrap()[0].value.as_deref(),Some("yes"));assert_eq!(pod.volumes.unwrap()[0].name,"keep");Ok(())
        }.await;
        namespaces
            .delete(&ns, &DeleteParams::default())
            .await
            .unwrap();
        result.unwrap();
    }
}
