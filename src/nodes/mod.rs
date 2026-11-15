//! Cluster-wide node administration; namespace application ownership does not apply.
mod routes;
mod service;
mod validation;
pub(crate) use routes::routes;
#[cfg(test)]
mod tests;
