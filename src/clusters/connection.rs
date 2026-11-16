use crate::{
    error::{Result, invalid},
    validation::valid_name,
};
use k8s_openapi::api::{
    apps::v1::Deployment,
    authorization::v1::{ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec},
};
use kube::{
    Api,
    api::{ListParams, PostParams},
};
use kube::{
    Client, Config,
    config::{KubeConfigOptions, Kubeconfig},
};
use serde_json::{Value, json};
use std::time::Duration;
pub(super) fn parse(raw: &str, context: &str, namespace: &str) -> Result<(Kubeconfig, String)> {
    if raw.len() > 512 * 1024 || !valid_name(namespace) {
        return Err(invalid(
            "Use a namespace name up to 63 characters and a kubeconfig smaller than 512 KiB.",
        ));
    }
    let config=Kubeconfig::from_yaml(raw).map_err(|_|invalid("Cannot parse kubeconfig. Upload a self-contained kubeconfig with embedded credentials."))?;
    for user in &config.auth_infos {
        if let Some(auth) = &user.auth_info {
            let value = serde_json::to_value(auth).map_err(|_| invalid("Invalid credentials."))?;
            if value.as_object().is_some_and(|m| {
                m.keys().any(|k| {
                    !matches!(
                        k.as_str(),
                        "token" | "client-certificate-data" | "client-key-data"
                    )
                })
            }) {
                return Err(invalid(
                    "Uploaded kubeconfigs may only contain inline tokens or client certificates/keys. Exec/auth-provider plugins, impersonation, local file references, and unknown auth options are not allowed.",
                ));
            }
        }
    }
    for named in &config.clusters {
        let Some(cluster) = &named.cluster else {
            continue;
        };
        if cluster.certificate_authority.is_some()
            || cluster.insecure_skip_tls_verify == Some(true)
            || cluster.proxy_url.is_some()
        {
            return Err(invalid(
                "Embed certificate-authority-data; local CA files, proxy URLs and disabled TLS verification are not allowed.",
            ));
        }
        let url = reqwest::Url::parse(cluster.server.as_deref().unwrap_or(""))
            .map_err(|_| invalid("Cluster server must be an HTTPS URL."))?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "Cluster server must be HTTPS without URL credentials or parameters.",
            ));
        }
    }
    let selected = if context.is_empty() {
        config.current_context.as_deref().unwrap_or("")
    } else {
        context
    };
    let selected_context = config
        .contexts
        .iter()
        .find(|c| c.name == selected)
        .and_then(|c| c.context.as_ref())
        .ok_or_else(|| invalid("Choose an existing kubeconfig context, or set current-context."))?;
    let server = config
        .clusters
        .iter()
        .find(|c| c.name == selected_context.cluster)
        .and_then(|c| c.cluster.as_ref())
        .and_then(|c| c.server.clone())
        .ok_or_else(|| invalid("Selected context has no cluster server."))?;
    Ok((config, server))
}
pub(super) async fn client(raw: &str, context: &str, namespace: &str) -> Result<(Client, String)> {
    let (config, server) = parse(raw, context, namespace)?;
    let options = KubeConfigOptions {
        context: if context.is_empty() {
            None
        } else {
            Some(context.into())
        },
        ..Default::default()
    };
    let mut config=Config::from_custom_kubeconfig(config,&options).await.map_err(|_|invalid("Cannot create a verified Kubernetes client from this kubeconfig. Check its embedded certificates and credentials."))?;
    config.default_namespace = namespace.into();
    config.connect_timeout = Some(Duration::from_secs(5));
    config.read_timeout = Some(Duration::from_secs(20));
    config.write_timeout = Some(Duration::from_secs(20));
    config.proxy_url = None;
    let client = Client::try_from(config)
        .map_err(|_| invalid("Cannot initialize the Kubernetes TLS client."))?;
    Ok((client, server))
}
pub(super) async fn test(client: Client, namespace: &str) -> Value {
    let version = match tokio::time::timeout(Duration::from_secs(12), client.apiserver_version())
        .await
    {
        Ok(Ok(v)) => v.git_version,
        _ => {
            return json!({"connected":false,"message":"Cannot reach the Kubernetes API with verified TLS. Check the server address, CA, certificate/token expiry, and network access from the KubeWeasel server.","permissions":[]});
        }
    };
    let namespace_read = Api::<Deployment>::namespaced(client.clone(), namespace)
        .list(&ListParams::default().limit(1))
        .await;
    let namespace_message = match namespace_read {
        Ok(_) => "Application namespace is readable.",
        Err(kube::Error::Api(ref e)) if e.code == 404 => {
            "The configured namespace does not exist. Create it before deploying applications."
        }
        Err(_) => "Namespace workloads are not readable with these credentials.",
    };
    let checks = [
        ("View workloads", "apps", "deployments", "list", true),
        ("Deploy applications", "apps", "deployments", "create", true),
        ("Update applications", "apps", "deployments", "patch", true),
        ("Stateful services", "apps", "statefulsets", "create", true),
        ("Application services", "", "services", "create", true),
        ("Pod logs", "", "pods/log", "get", true),
        ("Port forwarding", "", "pods/portforward", "create", true),
        (
            "Persistent volumes",
            "",
            "persistentvolumeclaims",
            "create",
            true,
        ),
        ("Build jobs", "batch", "jobs", "create", true),
        ("Registry credentials", "", "secrets", "create", true),
        (
            "Domain routes",
            "networking.k8s.io",
            "ingresses",
            "create",
            true,
        ),
        (
            "Read domain routes",
            "networking.k8s.io",
            "ingresses",
            "list",
            true,
        ),
        ("Node monitoring", "", "nodes", "list", false),
        ("Node management", "", "nodes", "patch", false),
        ("Node drain", "", "pods/eviction", "create", false),
        (
            "Storage class discovery",
            "storage.k8s.io",
            "storageclasses",
            "list",
            false,
        ),
    ];
    let reviews: Api<SelfSubjectAccessReview> = Api::all(client);
    let mut permissions = Vec::new();
    for (name, group, resource, verb, scoped) in checks {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    namespace: scoped.then(|| namespace.into()),
                    group: Some(group.into()),
                    resource: Some(resource.split('/').next().unwrap_or(resource).into()),
                    subresource: resource.split_once('/').map(|(_, s)| s.into()),
                    verb: Some(verb.into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let allowed = match reviews.create(&PostParams::default(), &review).await {
            Ok(r) => r.status.map(|s| s.allowed),
            Err(_) => None,
        };
        permissions.push(json!({"name":name,"resource":resource,"verb":verb,"allowed":allowed}));
    }
    json!({"connected":true,"version":version,"message":namespace_message,"permissions":permissions})
}
#[cfg(test)]
mod tests {
    use super::*;
    fn config(auth: &str) -> String {
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: test\nclusters:\n- name: test\n  cluster:\n    server: https://127.0.0.1:6443\ncontexts:\n- name: test\n  context:\n    cluster: test\n    user: test\nusers:\n- name: test\n  user:\n{auth}\n"
        )
    }
    #[test]
    fn rejects_executable_and_file_credentials() {
        for auth in [
            "    exec:\n      command: malicious",
            "    tokenFile: /etc/passwd",
            "    client-key: /tmp/key",
            "    auth-provider:\n      name: gcp",
            "    as: admin",
        ] {
            assert!(parse(&config(auth), "", "default").is_err());
        }
        assert!(parse(&config("    token: example"), "", "default").is_ok());
        for option in [
            "insecure-skip-tls-verify: true",
            "certificate-authority: /etc/private-ca",
            "proxy-url: http://proxy.test",
        ] {
            let raw = config("    token: example")
                .replace("    server:", &format!("    {option}\n    server:"));
            assert!(parse(&raw, "", "default").is_err());
        }
        let extra_user = config("    token: example")
            + "- name: unused\n  user:\n    exec:\n      command: malicious\n";
        assert!(parse(&extra_user, "", "default").is_err());
        assert!(parse(&config("    token: example"), "missing", "default").is_err());
        assert!(
            parse(
                &config("    token: example").replace("https://", "http://"),
                "",
                "default"
            )
            .is_err()
        );
    }
}
