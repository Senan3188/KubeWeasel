#[cfg(test)]
use crate::{
    applications::{self, models::Deploy},
    projects, storage,
};
use crate::{
    error::{Result, invalid},
    settings,
    state::AppState,
    validation::valid_name,
};
#[cfg(test)]
use axum::http::HeaderMap;
use axum::{
    Json, Router,
    extract::{Path, State},
    routing::get,
};
use kube::{
    Api, Client,
    api::{ApiResource, DynamicObject, GroupVersionKind, ListParams},
};
use rand_core::{OsRng, RngCore};
use serde_json::{Value, json};
use settings::Domain;
use std::collections::BTreeMap;
#[cfg(test)]
use std::sync::Arc;
#[cfg(test)]
use tokio::sync::RwLock;

const ISSUER: &str = "cert-manager.io/issuer";
const CLUSTER_ISSUER: &str = "cert-manager.io/cluster-issuer";
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/apps/{name}/https", get(status))
}
fn api(client: Client, ns: &str, kind: &str) -> Api<DynamicObject> {
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("cert-manager.io", "v1", kind));
    if kind == "ClusterIssuer" {
        Api::all_with(client, &ar)
    } else {
        Api::namespaced_with(client, ns, &ar)
    }
}
pub fn mode(d: &Domain) -> &str {
    if d.tls_mode.is_empty() {
        if d.tls_secret.is_empty() {
            "http"
        } else {
            "existing"
        }
    } else {
        &d.tls_mode
    }
}
pub fn normalize(name: &str, d: &mut Domain, previous: &Domain) -> Result<()> {
    if !matches!(mode(d), "http" | "existing" | "automatic") {
        return Err(invalid(
            "Choose HTTP only, Automatic HTTPS, or Existing certificate.",
        ));
    }
    d.tls_mode = mode(d).into();
    if d.host.is_empty() || d.tls_mode == "http" {
        d.tls_secret.clear();
        d.issuer.clear();
        d.issuer_kind.clear();
        return Ok(());
    }
    if d.tls_mode == "existing" {
        if d.tls_secret.is_empty() {
            return Err(invalid("Enter the existing TLS Secret name."));
        }
        d.issuer.clear();
        d.issuer_kind.clear();
        return Ok(());
    }
    if d.issuer.is_empty()
        || d.issuer.len() > 253
        || !d.issuer.split('.').all(valid_name)
        || !matches!(d.issuer_kind.as_str(), "Issuer" | "ClusterIssuer")
    {
        return Err(invalid(
            "Select a namespaced Issuer or ClusterIssuer for Automatic HTTPS.",
        ));
    }
    if d.ingress_class.is_empty() {
        return Err(invalid("Select an ingress class for Automatic HTTPS."));
    }
    d.tls_secret = if mode(previous) == "automatic" && !previous.tls_secret.is_empty() {
        previous.tls_secret.clone()
    } else {
        let mut bytes = [0u8; 8];
        OsRng.fill_bytes(&mut bytes);
        format!(
            "{}-{}-tls",
            name.chars().take(20).collect::<String>(),
            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
        )
    };
    Ok(())
}
pub fn annotations(d: &Domain) -> Value {
    json!({ISSUER:if mode(d)=="automatic"&&d.issuer_kind=="Issuer"{json!(d.issuer)}else{Value::Null},CLUSTER_ISSUER:if mode(d)=="automatic"&&d.issuer_kind=="ClusterIssuer"{json!(d.issuer)}else{Value::Null},"acme.cert-manager.io/http01-ingress-ingressclassname":if mode(d)=="automatic"{json!(d.ingress_class)}else{Value::Null}})
}
pub fn read_annotations(d: &mut Domain, annotations: &BTreeMap<String, String>) {
    if let Some(name) = annotations.get(ISSUER) {
        d.tls_mode = "automatic".into();
        d.issuer = name.clone();
        d.issuer_kind = "Issuer".into();
    } else if let Some(name) = annotations.get(CLUSTER_ISSUER) {
        d.tls_mode = "automatic".into();
        d.issuer = name.clone();
        d.issuer_kind = "ClusterIssuer".into();
    }
}
fn conditions(object: &Value) -> Vec<Value> {
    object
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}
fn ready(object: &Value) -> bool {
    let generation = object
        .pointer("/metadata/generation")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    conditions(object).iter().any(|c| {
        c["type"] == "Ready"
            && c["status"] == "True"
            && c["observedGeneration"].as_i64().unwrap_or(0) >= generation
    })
}
fn reason(object: &Value) -> String {
    conditions(object)
        .iter()
        .filter(|c| c["type"] == "Ready" || c["type"] == "Issuing")
        .map(|c| {
            format!(
                "{}: {}",
                c["reason"].as_str().unwrap_or("Pending"),
                c["message"].as_str().unwrap_or("Waiting for cert-manager.")
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn json_object(object: DynamicObject) -> Value {
    serde_json::to_value(object).unwrap_or(Value::Null)
}
pub async fn require_automatic(s: &AppState, d: &Domain) -> Result<()> {
    if mode(d) != "automatic" || d.host.is_empty() {
        return Ok(());
    }
    let Some(client) = s.client.clone() else {
        return Ok(());
    };
    api(client.clone(),&s.namespace,"Certificate").list(&ListParams::default().limit(1)).await.map_err(|_|invalid("Automatic HTTPS needs cert-manager's Certificate API and read permissions. See certificate setup guidance."))?;
    let issuer = api(client, &s.namespace, &d.issuer_kind)
        .get(&d.issuer)
        .await
        .map(json_object)
        .map_err(|_| invalid("Selected issuer is missing or cannot be read."))?;
    if !ready(&issuer) {
        return Err(invalid(&format!(
            "Selected issuer is not ready. {}",
            reason(&issuer)
        )));
    }
    if issuer.pointer("/spec/acme").is_some()
        && (d.host.parse::<std::net::IpAddr>().is_ok()
            || !d.host.contains('.')
            || [
                ".test",
                ".localhost",
                ".local",
                ".invalid",
                ".internal",
                ".example",
            ]
            .iter()
            .any(|suffix| d.host.ends_with(suffix)))
    {
        return Err(invalid(
            "ACME certificates require a public domain. Local .test names need a local test issuer or an existing certificate.",
        ));
    }
    Ok(())
}
fn certificate_state(c: &Value, d: &Domain) -> Value {
    let expected = c.pointer("/spec/secretName").and_then(Value::as_str) == Some(&d.tls_secret)
        && c.pointer("/spec/dnsNames")
            .and_then(Value::as_array)
            .is_some_and(|names| names.iter().any(|n| n.as_str() == Some(&d.host)))
        && c.pointer("/spec/issuerRef/name").and_then(Value::as_str) == Some(&d.issuer)
        && c.pointer("/spec/issuerRef/kind")
            .and_then(Value::as_str)
            .unwrap_or("Issuer")
            == d.issuer_kind;
    if !expected {
        return json!({"state":"pending","title":"Updating certificate request","message":"Certificate configuration does not yet match the saved domain. Waiting for cert-manager to reconcile it."});
    }
    let renewing = conditions(c)
        .iter()
        .any(|v| v["type"] == "Issuing" && v["status"] == "True");
    if ready(c) {
        json!({"state":"ready","title":if renewing{"Certificate ready · renewing"}else{"Certificate ready"},"message":"cert-manager reports a ready certificate. Use Test connection to verify DNS, routing, and the certificate served to clients.","not_after":c.pointer("/status/notAfter"),"renewal_time":c.pointer("/status/renewalTime")})
    } else {
        let failed = c
            .pointer("/status/failedIssuanceAttempts")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            > 0;
        json!({"state":if failed{"error"}else{"pending"},"title":if failed{"Certificate issuance needs attention"}else{"Issuing certificate"},"message":reason(c),"not_after":c.pointer("/status/notAfter"),"renewal_time":c.pointer("/status/renewalTime")})
    }
}
async fn status(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    let cfg = settings::load(&s, &name).await?;
    Ok(Json(inspect(&s, &cfg.domain).await))
}
pub async fn inspect(s: &AppState, d: &Domain) -> Value {
    let mut output = json!({"mode":mode(d),"host":d.host,"secret":d.tls_secret,"issuers":[],"state":"http","title":"HTTP only","message":"Choose Automatic HTTPS to request and renew certificates with cert-manager.","available":false});
    let Some(client) = s.client.clone() else {
        output["state"] = json!("demo");
        output["title"] = json!("Live cluster required");
        output["message"] = json!("Demo mode does not request certificates.");
        return output;
    };
    let certificates = api(client.clone(), &s.namespace, "Certificate");
    let probe = certificates.list(&ListParams::default().limit(1)).await;
    match probe {
        Ok(_) => {
            output["available"] = json!(true);
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            output["state"] = json!("missing");
            output["title"] = json!("cert-manager not installed");
            output["message"] = json!(
                "Install cert-manager and configure an issuer before requesting Automatic HTTPS. HTTP and existing TLS Secrets remain usable."
            );
            return output;
        }
        Err(_) => {
            output["state"] = json!("unknown");
            output["title"] = json!("Cannot inspect cert-manager");
            output["message"] = json!(
                "Check Certificate API availability and read permissions. This does not prove cert-manager is missing."
            );
            return output;
        }
    }
    let (issuers, cluster) = tokio::join!(
        async {
            api(client.clone(), &s.namespace, "Issuer")
                .list(&ListParams::default())
                .await
        },
        async {
            api(client.clone(), &s.namespace, "ClusterIssuer")
                .list(&ListParams::default())
                .await
        }
    );
    let mut choices = Vec::new();
    let mut warnings = Vec::new();
    for (kind, result) in [("Issuer", issuers), ("ClusterIssuer", cluster)] {
        match result {
            Ok(list) => {
                for i in list.items {
                    let value = json_object(i);
                    let server = value.pointer("/spec/acme/server").and_then(Value::as_str);
                    choices.push(json!({"name":value["metadata"]["name"],"kind":kind,"ready":ready(&value),"message":reason(&value),"staging":server.is_some_and(|v|v.contains("acme-staging")),"acme":server.is_some()}));
                }
            }
            Err(_) => warnings.push(format!(
                "Cannot list {kind} resources; check discovery permissions."
            )),
        }
    }
    output["issuers"] = json!(choices);
    output["warnings"] = json!(warnings);
    if mode(d) == "existing" {
        output["state"] = json!("existing");
        output["title"] = json!("Existing certificate");
        output["message"] = json!(
            "Using the configured TLS Secret. Its contents are not read; Test connection validates the certificate actually served."
        );
        return output;
    }
    if mode(d) != "automatic" || d.host.is_empty() {
        return output;
    }
    let issuer = api(client, &s.namespace, &d.issuer_kind)
        .get(&d.issuer)
        .await;
    match issuer {
        Ok(i) => {
            let value = json_object(i);
            output["staging"] = json!(
                value
                    .pointer("/spec/acme/server")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("acme-staging"))
            );
            output["acme"] = json!(value.pointer("/spec/acme").is_some());
            if !ready(&value) {
                output["state"] = json!("pending");
                output["title"] = json!("Issuer not ready");
                output["message"] = json!(reason(&value));
                return output;
            }
        }
        Err(_) => {
            output["state"] = json!("unknown");
            output["title"] = json!("Issuer unavailable");
            output["message"] = json!("The saved issuer is missing or cannot be read.");
            return output;
        }
    }
    match certificates.get_opt(&d.tls_secret).await {
        Ok(Some(c)) => {
            let state = certificate_state(&json_object(c), d);
            for (k, v) in state.as_object().unwrap() {
                output[k] = v.clone();
            }
        }
        Ok(None) => {
            output["state"] = json!("pending");
            output["title"] = json!("Waiting for certificate request");
            output["message"] = json!(
                "Ingress saved. Waiting for cert-manager ingress-shim to create a Certificate. Check that cert-manager is running."
            );
        }
        Err(_) => {
            output["state"] = json!("unknown");
            output["title"] = json!("Certificate status unavailable");
            output["message"] = json!(
                "Cannot read the Certificate resource. Check permissions and cert-manager availability."
            );
        }
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    fn domain() -> Domain {
        Domain {
            host: "app.example.com".into(),
            path: "/".into(),
            ingress_class: "traefik".into(),
            tls_mode: "automatic".into(),
            issuer: "test".into(),
            issuer_kind: "Issuer".into(),
            ..Default::default()
        }
    }
    #[test]
    fn normalizes_legacy_and_switches_modes() {
        let mut d = Domain {
            tls_secret: "existing".into(),
            ..Default::default()
        };
        assert_eq!(mode(&d), "existing");
        d.tls_mode = "http".into();
        normalize("app", &mut d, &Domain::default()).unwrap();
        assert!(d.tls_secret.is_empty());
        let mut auto = domain();
        normalize("app", &mut auto, &Domain::default()).unwrap();
        let secret = auto.tls_secret.clone();
        let prev = auto.clone();
        normalize("app", &mut auto, &prev).unwrap();
        assert_eq!(secret, auto.tls_secret);
        assert!(annotations(&auto)[CLUSTER_ISSUER].is_null());
        assert_eq!(annotations(&auto)[ISSUER], "test");
    }
    #[test]
    fn rejects_invalid_modes_and_issuers() {
        let mut d = domain();
        d.issuer_kind = "Bogus".into();
        assert!(normalize("app", &mut d, &Domain::default()).is_err());
        d.issuer_kind = "Issuer".into();
        d.tls_mode = "magic".into();
        assert!(normalize("app", &mut d, &Domain::default()).is_err());
    }
    #[test]
    fn stale_ready_is_not_reported_as_ready() {
        let mut d = domain();
        d.tls_secret = "app-tls".into();
        let mut c = json!({"metadata":{"generation":2},"spec":{"secretName":"app-tls","dnsNames":[d.host],"issuerRef":{"name":"test","kind":"Issuer"}},"status":{"conditions":[{"type":"Ready","status":"True","observedGeneration":1}]}});
        assert_eq!(certificate_state(&c, &d)["state"], "pending");
        c["status"]["conditions"][0]["observedGeneration"] = json!(2);
        assert_eq!(certificate_state(&c, &d)["state"], "ready");
        d.host = "other.example.com".into();
        assert_eq!(certificate_state(&c, &d)["state"], "pending");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use k8s_openapi::api::core::v1::Namespace;
    use kube::api::{DeleteParams, PostParams};
    #[tokio::test]
    #[ignore = "creates an isolated namespace and self-signed cert-manager resources; requires a local test cluster"]
    async fn cert_manager_issues_and_modes_round_trip() {
        assert_eq!(
            std::env::var("KUBEWEASEL_RUN_HTTPS_TEST").as_deref(),
            Ok("1"),
            "Set KUBEWEASEL_RUN_HTTPS_TEST=1 explicitly."
        );
        let client = Client::try_default().await.unwrap();
        let mut random = [0u8; 6];
        OsRng.fill_bytes(&mut random);
        let ns = format!(
            "kw-https-check-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let namespaces: Api<Namespace> = Api::all(client.clone());
        let object:Namespace=serde_json::from_value(json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":ns,"labels":{"app.kubernetes.io/managed-by":"kubeweasel-test"}}})).unwrap();
        namespaces
            .create(&PostParams::default(), &object)
            .await
            .unwrap();
        let outcome:std::result::Result<(),Box<dyn std::error::Error>>=async{
   let issuer_api=api(client.clone(),&ns,"Issuer");let issuer:DynamicObject=serde_json::from_value(json!({"apiVersion":"cert-manager.io/v1","kind":"Issuer","metadata":{"name":"local-selfsigned"},"spec":{"selfSigned":{}}})).unwrap();issuer_api.create(&PostParams::default(),&issuer).await?;
   for attempt in 0..60{if ready(&json_object(issuer_api.get("local-selfsigned").await?)){break;}
   if attempt==59{return Err("Test issuer did not become ready".into());}tokio::time::sleep(std::time::Duration::from_secs(1)).await;}
   let s=AppState{build_gate:Default::default(),client:Some(client.clone()),namespace:ns.clone(),apps:Arc::new(RwLock::new(BTreeMap::new())),settings:settings::SettingsState::default(),storage:storage::StorageState::default(),projects:projects::Store::open(":memory:")?};let mut headers=HeaderMap::new();headers.insert("x-kubeweasel-request","dashboard".parse().unwrap());applications::service::deploy(&s, Deploy{workload_kind:String::new(),project_id:None,name:"https-check".into(),image:"nginx:stable".into(),port:80,replicas:0}).await.map_err(|e|e.1)?;
   let mut d=Domain{host:"https-check.test".into(),path:"/".into(),ingress_class:"traefik".into(),tls_mode:"automatic".into(),issuer:"local-selfsigned".into(),issuer_kind:"Issuer".into(),..Default::default()};let _=settings::domain(State(s.clone()),Path("https-check".into()),headers.clone(),Json(d.clone())).await.map_err(|e|e.1)?;
   let loaded=settings::load(&s,"https-check").await.map_err(|e|e.1)?;if mode(&loaded.domain)!="automatic"||loaded.domain.issuer!="local-selfsigned"{return Err("Automatic HTTPS settings did not survive readback".into());}
   for attempt in 0..90{let result=inspect(&s,&loaded.domain).await;if result["state"]=="ready"{break;}
   if attempt==89{return Err(format!("Certificate not ready: {result}").into());}tokio::time::sleep(std::time::Duration::from_secs(1)).await;}
   d.tls_mode="http".into();let _=settings::domain(State(s.clone()),Path("https-check".into()),headers,Json(d)).await.map_err(|e|e.1)?;let loaded=settings::load(&s,"https-check").await.map_err(|e|e.1)?;if mode(&loaded.domain)!="http"||!loaded.domain.tls_secret.is_empty(){return Err("HTTP-only switch retained automatic TLS configuration".into());}
   Ok(())
  }.await;
        namespaces
            .delete(&ns, &DeleteParams::default())
            .await
            .unwrap();
        outcome.unwrap();
    }
}
