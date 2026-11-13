//! Kubernetes resource construction, without API calls or request handling.
use super::models::Deploy;
use k8s_openapi::{
    api::{
        apps::v1::{Deployment, DeploymentSpec},
        core::v1::{
            Container, ContainerPort, LocalObjectReference, PodSpec, PodTemplateSpec,
            ResourceRequirements, Service, ServicePort, ServiceSpec,
        },
    },
    apimachinery::pkg::{
        api::resource::Quantity,
        apis::meta::v1::{LabelSelector, ObjectMeta},
        util::intstr::IntOrString,
    },
};
use std::collections::BTreeMap;

pub(super) fn deployment(
    input: &Deploy,
    labels: BTreeMap<String, String>,
    pull_secret: Option<String>,
) -> Deployment {
    let selector = BTreeMap::from([("app.kubernetes.io/name".into(), input.name.clone())]);
    let mut pod_labels = selector.clone();
    pod_labels.insert("app.kubernetes.io/managed-by".into(), "kubeweasel".into());
    Deployment {
        metadata: ObjectMeta {
            name: Some(input.name.clone()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(input.replicas),
            selector: LabelSelector {
                match_labels: Some(selector),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(pod_labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    containers: vec![Container {
                        name: "app".into(),
                        image: Some(input.image.clone()),
                        ports: Some(vec![ContainerPort {
                            container_port: i32::from(input.port),
                            ..Default::default()
                        }]),
                        resources: Some(ResourceRequirements {
                            requests: Some(BTreeMap::from([
                                ("cpu".into(), Quantity("100m".into())),
                                ("memory".into(), Quantity("64Mi".into())),
                            ])),
                            limits: Some(BTreeMap::from([
                                ("cpu".into(), Quantity("1".into())),
                                ("memory".into(), Quantity("512Mi".into())),
                            ])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }],
                    image_pull_secrets: pull_secret.map(|name| vec![LocalObjectReference { name }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}
pub(super) fn service(input: &Deploy, labels: BTreeMap<String, String>, kind: &str) -> Service {
    Service {
        metadata: ObjectMeta {
            name: Some(input.name.clone()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                "app.kubernetes.io/name".into(),
                input.name.clone(),
            )])),
            ports: Some(vec![ServicePort {
                port: i32::from(input.port),
                target_port: Some(IntOrString::Int(i32::from(input.port))),
                ..Default::default()
            }]),
            cluster_ip: if kind == "StatefulSet" {
                Some("None".into())
            } else {
                None
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn input() -> Deploy {
        Deploy {
            workload_kind: "Deployment".into(),
            project_id: None,
            name: "web".into(),
            image: "nginx:stable".into(),
            port: 80,
            replicas: 1,
        }
    }
    #[test]
    fn credentials_attached_before_workload_creation() {
        let d = deployment(&input(), BTreeMap::new(), Some("pull-test".into()));
        assert_eq!(
            d.spec
                .unwrap()
                .template
                .spec
                .unwrap()
                .image_pull_secrets
                .unwrap()[0]
                .name,
            "pull-test"
        );
    }
    #[test]
    fn only_stateful_services_are_headless() {
        assert_eq!(
            service(&input(), BTreeMap::new(), "StatefulSet")
                .spec
                .unwrap()
                .cluster_ip
                .as_deref(),
            Some("None")
        );
        assert!(
            service(&input(), BTreeMap::new(), "Deployment")
                .spec
                .unwrap()
                .cluster_ip
                .is_none()
        );
    }
}
