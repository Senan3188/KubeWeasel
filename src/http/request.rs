use crate::error::{Error, Result};
use axum::http::{HeaderMap, StatusCode};

pub(crate) fn mutation(headers: &HeaderMap) -> Result<()> {
    if headers
        .get("x-kubeweasel-request")
        .and_then(|h| h.to_str().ok())
        != Some("dashboard")
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Missing dashboard request header.".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requires_custom_header() {
        assert!(mutation(&HeaderMap::new()).is_err());
    }
}
