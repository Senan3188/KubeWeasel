//! Readiness probe: deliberately does not open the account DB or Kubernetes client.
use crate::config::Config;
pub(crate) async fn check(config: &Config) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let bind: std::net::SocketAddr = config.address.parse()?;
    let ip = if bind.ip().is_unspecified() {
        if bind.is_ipv4() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
    } else {
        bind.ip()
    };
    let mut request = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(format!(
            "http://{}/healthz",
            std::net::SocketAddr::new(ip, bind.port())
        ));
    if let Some(origin) = config
        .public_origin
        .as_ref()
        .or(config.private_origin.as_ref())
    {
        let origin = reqwest::Url::parse(origin)?.origin().ascii_serialization();
        request = request.header(
            "host",
            origin.split_once("://").map(|(_, host)| host).unwrap_or(""),
        );
    }
    request.send().await?.error_for_status()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn private_origin_probe_preserves_host_without_opening_account_storage() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let router = axum::Router::new().route(
            "/healthz",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                assert_eq!(headers["host"], "deploy.example.internal:7878");
                "ok"
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let config = Config {
            address,
            auth_db: "/unavailable/account.db".into(),
            public_origin: None,
            private_origin: Some("http://deploy.example.internal:7878".into()),
            namespace: "unavailable".into(),
            demo: false,
            healthcheck: true,
        };
        check(&config).await.unwrap();
        server.abort();
    }
}
