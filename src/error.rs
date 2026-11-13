use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub(crate) struct Error(pub(crate) StatusCode, pub(crate) String);
impl Error {
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }
    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, message.into())
    }
    pub(crate) fn bad_gateway(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_GATEWAY, message.into())
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
impl From<kube::Error> for Error {
    fn from(e: kube::Error) -> Self {
        Self(StatusCode::BAD_GATEWAY, e.to_string())
    }
}
pub(crate) type Result<T> = std::result::Result<T, Error>;
pub(crate) fn invalid(message: &str) -> Error {
    Error(StatusCode::BAD_REQUEST, message.into())
}
