use crate::{
    error::{Result, invalid},
    http::request::mutation,
    https, settings,
    state::AppState,
    workloads::managed,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::{get, post},
};
use k8s_openapi::api::{
    core::v1::Service,
    networking::v1::{Ingress, IngressClass},
};
use kube::{Api, ResourceExt, api::ListParams};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps/{name}/networking/diagnostics", get(diagnostics))
        .route("/api/apps/{name}/networking/test", post(test_connection))
}
#[derive(Serialize)]
struct Check {
    id: &'static str,
    title: &'static str,
    state: &'static str,
    message: String,
}
fn check(
    id: &'static str,
    title: &'static str,
    state: &'static str,
    message: impl Into<String>,
) -> Check {
    Check {
        id,
        title,
        state,
        message: message.into(),
    }
}
async fn dns(host: &str) -> std::result::Result<Vec<IpAddr>, String> {
    let addresses =
        tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((host, 80)))
            .await
            .map_err(|_| "DNS lookup timed out.".to_string())?
            .map_err(|_| "This hostname does not resolve on the KubeWeasel server.".to_string())?;
    let mut ips = addresses.map(|a| a.ip()).collect::<Vec<_>>();
    ips.sort();
    ips.dedup();
    if ips.is_empty() {
        Err("No addresses found.".into())
    } else {
        Ok(ips)
    }
}
fn dns_check(result: &std::result::Result<Vec<IpAddr>, String>) -> Check {
    match result {
        Ok(ips) => check(
            "dns",
            "DNS resolution",
            "pass",
            format!(
                "Server resolves to {}. Your browser may use different DNS.",
                ips.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Err(e) => check(
            "dns",
            "DNS resolution",
            "warning",
            format!(
                "{e} For a local test, add 127.0.0.1 <hostname> to your computer’s hosts file, or use the local test option below."
            ),
        ),
    }
}
async fn diagnostics(State(s): State<AppState>, Path(name): Path<String>) -> Result<Json<Value>> {
    let cfg = settings::load(&s, &name).await?;
    let d = &cfg.domain;
    if s.client.is_none() {
        return Ok(Json(
            json!({"demo":true,"checks":[check("demo","Demo workspace","info","Diagnostics need a live Kubernetes connection.")],"host":d.host}),
        ));
    }
    let client = s.client.clone().unwrap();
    let mut checks = Vec::new();
    let api = managed(&s, &name).await?;
    let deployment = api.get(&name).await?;
    let ready = deployment
        .status
        .as_ref()
        .and_then(|s| s.ready_replicas)
        .unwrap_or(0);
    checks.push(check(
        "backend",
        "Application readiness",
        if ready > 0 { "pass" } else { "warning" },
        if ready > 0 {
            format!(
                "{ready} ready replicas. A connection test checks the HTTP response separately."
            )
        } else {
            "No ready replicas. Scale the app up or inspect its logs before testing.".into()
        },
    ));
    let services: Api<Service> = Api::namespaced(client.clone(), &s.namespace);
    match services.get_opt(&name).await {
        Ok(Some(service)) => {
            let owned = service
                .labels()
                .get("app.kubernetes.io/managed-by")
                .map(String::as_str)
                == Some("kubeweasel");
            checks.push(check(
                "service",
                "Internal Service",
                if owned { "pass" } else { "warning" },
                if owned {
                    "Managed Service exists."
                } else {
                    "Service exists but is not managed by KubeWeasel."
                },
            ));
        }
        Ok(None) => checks.push(check(
            "service",
            "Internal Service",
            "warning",
            "Service is missing. Domain routing needs a backend Service.",
        )),
        Err(_) => checks.push(check(
            "service",
            "Internal Service",
            "unknown",
            "Could not inspect Service. Check Kubernetes permissions.",
        )),
    }
    let classes: Api<IngressClass> = Api::all(client.clone());
    let class_info = match classes.list(&ListParams::default()).await {
        Ok(list) => {
            let names = list
                .items
                .iter()
                .map(ResourceExt::name_any)
                .collect::<Vec<_>>();
            let matches = list
                .items
                .iter()
                .filter(|i| {
                    if d.ingress_class.is_empty() {
                        i.annotations()
                            .get("ingressclass.kubernetes.io/is-default-class")
                            .map(String::as_str)
                            == Some("true")
                    } else {
                        i.name_any() == d.ingress_class
                    }
                })
                .collect::<Vec<_>>();
            if let [i] = matches.as_slice() {
                checks.push(check("controller","Ingress controller class","pass",format!("{} → {}. Registration does not prove the controller is running; use Test connection.",i.name_any(),i.spec.as_ref().and_then(|s|s.controller.as_deref()).unwrap_or("unknown"))));
            } else if list.items.is_empty() {
                checks.push(check("controller","Ingress controller class","warning","No ingress classes registered. Install an ingress controller such as Traefik before using a domain."));
            } else {
                checks.push(check(
                    "controller",
                    "Ingress controller class",
                    "warning",
                    if d.ingress_class.is_empty() {
                        "Choose an ingress class explicitly; there is no single default class."
                            .into()
                    } else {
                        format!(
                            "Class '{}' was not found. Choose an installed class.",
                            d.ingress_class
                        )
                    },
                ));
            }
            names
        }
        Err(_) => {
            checks.push(check("controller","Ingress controller class","unknown","Cannot list cluster-scoped ingress classes. Ask your administrator for get/list access to ingressclasses, or verify with kubectl get ingressclass."));
            Vec::new()
        }
    };
    let mut addresses = Vec::new();
    if d.host.is_empty() {
        checks.push(check(
            "route",
            "Domain route",
            "warning",
            "No domain saved. Enter a hostname below and save the domain first.",
        ));
    } else {
        let ingresses: Api<Ingress> = Api::namespaced(client, &s.namespace);
        match ingresses.get_opt(&name).await {
            Ok(Some(i)) => {
                addresses = i
                    .status
                    .and_then(|s| s.load_balancer)
                    .and_then(|s| s.ingress)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|i| i.ip.or(i.hostname))
                    .collect();
                checks.push(check(
                    "route",
                    "Domain route",
                    "pass",
                    format!(
                        "Ingress saved for {}{}. This confirms configuration, not reachability.",
                        d.host, d.path
                    ),
                ));
            }
            Ok(None) => checks.push(check(
                "route",
                "Domain route",
                "warning",
                "Ingress is missing.",
            )),
            Err(_) => checks.push(check(
                "route",
                "Domain route",
                "unknown",
                "Cannot inspect the Ingress.",
            )),
        }
        checks.push(dns_check(&dns(&d.host).await));
    }
    checks.push(check("address","Ingress address",if addresses.is_empty(){"info"}else{"pass"},if addresses.is_empty(){"No external address advertised. This can be normal with local port forwarding; it does not confirm a routing failure.".into()}else{format!("Point your DNS A/AAAA or CNAME records to: {}",addresses.join(", "))}));
    checks.push(check("tls","HTTPS",if d.tls_secret.is_empty(){"info"}else{"unknown"},if d.tls_secret.is_empty(){"HTTP only. Configure a TLS Secret to enable HTTPS. A local port forward does not add TLS.".into()}else if https::mode(d)=="automatic"{"Automatic HTTPS is configured. See the certificate status panel for issuance and renewal; Test connection validates the certificate served.".into()}else{format!("Ingress references TLS Secret '{}'. Its existence, expiry, and hostname coverage are not verified here. Test HTTPS to validate the served certificate.",d.tls_secret)}));
    Ok(Json(
        json!({"demo":false,"host":d.host,"path":d.path,"scheme":if d.tls_secret.is_empty(){"http"}else{"https"},"classes":class_info,"addresses":addresses,"checks":checks}),
    ))
}
#[derive(Deserialize)]
struct TestRequest {
    port: u16,
    #[serde(default)]
    local: bool,
}
fn test_url(
    host: &str,
    path: &str,
    https: bool,
    port: u16,
) -> std::result::Result<reqwest::Url, String> {
    let mut url = reqwest::Url::parse(&format!(
        "{}://{host}",
        if https { "https" } else { "http" }
    ))
    .map_err(|_| "Invalid saved hostname.".to_string())?;
    // Treat the saved ingress path strictly as a path, never as another URL.
    url.set_path(if path.is_empty() { "/" } else { path });
    url.set_port(Some(port))
        .map_err(|_| "Invalid port.".to_string())?;
    Ok(url)
}
async fn test_connection(
    State(s): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(t): Json<TestRequest>,
) -> Result<Json<Value>> {
    mutation(&headers)?;
    let cfg = settings::load(&s, &name).await?;
    if s.client.is_none() {
        return Err(invalid(
            "Connection tests need a live Kubernetes connection.",
        ));
    }
    if cfg.domain.host.is_empty() {
        return Err(invalid("Save a hostname before testing."));
    }
    if t.port == 0 {
        return Err(invalid("Port must be 1–65535."));
    }
    let host = &cfg.domain.host;
    let https = !cfg.domain.tls_secret.is_empty();
    let url = test_url(host, &cfg.domain.path, https, t.port).map_err(|e| invalid(&e))?;
    let result = dns(host).await;
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .connect_timeout(Duration::from_secs(4));
    if t.local {
        builder = builder.resolve(
            host,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), t.port),
        );
    }
    let client = builder
        .build()
        .map_err(|_| invalid("Could not initialize the connection test."))?;
    let start = std::time::Instant::now();
    let (state, message, status) = match client
        .get(url.clone())
        .header("user-agent", "KubeWeasel-network-check/0.1")
        .send()
        .await
    {
        Ok(r) => {
            let status = r.status().as_u16();
            let (state, message) = http_result(status);
            (state, message, Some(status))
        }
        Err(e) => {
            let message = if e.is_timeout() {
                "Connection timed out. Check the controller, forwarding port, firewall, and DNS."
            } else if !t.local && result.is_err() {
                "Hostname does not resolve on the KubeWeasel server. Configure DNS or use the local loopback test."
            } else if https {
                "HTTPS connection failed. Check the TLS Secret, certificate trust/hostname, and HTTPS listener. HTTP forwarding ports cannot serve HTTPS."
            } else {
                "Could not connect. Check that the ingress controller is running and the selected port is exposed. For local testing, keep the Traefik port-forward command running."
            };
            ("warning", message.to_string(), None)
        }
    };
    Ok(Json(
        json!({"state":state,"message":message,"http_status":status,"elapsed_ms":start.elapsed().as_millis(),"url":url.as_str(),"local":t.local,"dns":dns_check(&result),"scope":if t.local{"Tested from the KubeWeasel server through 127.0.0.1, using the saved hostname for routing. This does not verify DNS or browser access."}else{"Tested from the KubeWeasel server using its DNS. Your browser may have different network access."}}),
    ))
}
fn http_result(status: u16) -> (&'static str, String) {
    match status{200..=299=>("pass",format!("HTTP {status}: the endpoint responded successfully. Verify that it is your expected application.")),300..=399=>("info",format!("HTTP {status}: endpoint redirected. Redirects are not followed; the destination is not verified.")),404=>("warning","HTTP 404: the controller or application returned Not Found. Check hostname, path, and ingress class.".into()),502..=504=>("warning",format!("HTTP {status}: gateway/backend failure. Check Service ports and ready pods.")),_=>("warning",format!("HTTP {status}: the endpoint responded, but did not return a successful application response."))}
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_test_keeps_host_and_path() {
        let url = test_url("hello.test", "//other.test/path", false, 8082).unwrap();
        assert_eq!(url.host_str(), Some("hello.test"));
        assert_eq!(url.port(), Some(8082));
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.path(), "//other.test/path");
    }
    #[test]
    fn tls_uses_https() {
        assert_eq!(
            test_url("app.test", "/app", true, 443).unwrap().scheme(),
            "https"
        );
    }
    #[test]
    fn http_status_is_not_blanket_success() {
        assert_eq!(http_result(200).0, "pass");
        assert_eq!(http_result(302).0, "info");
        assert_eq!(http_result(404).0, "warning");
        assert_eq!(http_result(503).0, "warning");
    }
    #[test]
    fn dns_failure_explains_local_setup() {
        let c = dns_check(&Err("No record".into()));
        assert_eq!(c.state, "warning");
        assert!(c.message.contains("hosts file"));
    }
}
