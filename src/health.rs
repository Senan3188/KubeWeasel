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
        .timeout(std::time::Duration::from_secs(2))
        .build()?
        .get(format!(
            "http://{}/healthz",
            std::net::SocketAddr::new(ip, bind.port())
        ));
    if let Some(origin) = &config.public_origin {
        let origin = reqwest::Url::parse(origin)?.origin().ascii_serialization();
        request = request.header("host", origin.trim_start_matches("https://"));
    }
    request.send().await?.error_for_status()?;
    Ok(())
}
