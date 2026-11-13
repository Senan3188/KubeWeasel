use crate::{
    applications::models::Deploy,
    error::{Result, invalid},
};

pub(crate) fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
}
pub(crate) fn validate(d: &Deploy) -> Result<()> {
    if !matches!(d.workload_kind.as_str(), "" | "Deployment" | "StatefulSet")
        || (d.workload_kind == "StatefulSet" && d.replicas > 1)
    {
        return Err(invalid(
            "Choose Deployment or a single-replica StatefulSet.",
        ));
    }
    if !valid_name(&d.name) {
        return Err(invalid("Use a lowercase DNS name, up to 63 characters."));
    }
    if d.image.is_empty() || d.image.len() > 512 || d.image.chars().any(char::is_whitespace) {
        return Err(invalid("Enter a valid container image reference."));
    }
    if d.port == 0 || !(0..=20).contains(&d.replicas) {
        return Err(invalid("Port must be 1–65535; replicas must be 0–20."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {

    use crate::applications::models::Deploy;

    use crate::validation::valid_name;
    use crate::validation::validate;

    #[test]
    fn rejects_invalid_resource_names() {
        for n in ["", "Hello", "-app", "app-", "a/b", "app.name"] {
            assert!(!valid_name(n));
        }
        assert!(valid_name("payments-api-2"));
    }
    #[test]
    fn bounds_deployments() {
        let mut d = Deploy {
            workload_kind: String::new(),
            project_id: None,
            name: "web".into(),
            image: "nginx:stable".into(),
            port: 80,
            replicas: 2,
        };
        assert!(validate(&d).is_ok());
        d.replicas = 21;
        assert!(validate(&d).is_err());
        d.replicas = 1;
        d.port = 0;
        assert!(validate(&d).is_err());
    }
}
