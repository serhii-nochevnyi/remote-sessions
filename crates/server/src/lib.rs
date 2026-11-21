use axum::{Json, Router, http::header, routing::get};

pub fn app() -> Router {
    Router::new().route(
        "/healthz",
        get(|| async {
            (
                [
                    (header::CACHE_CONTROL, "no-store"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                Json(remote_sessions_protocol::scaffold_status()),
            )
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn liveness_does_not_expose_session_execution() {
        for (method, path, code) in [
            ("GET", "/healthz", 200),
            ("POST", "/healthz", 405),
            ("GET", "/v1/sessions", 404),
            ("POST", "/v1/sessions", 404),
            ("GET", "/terminal/connect", 404),
            ("GET", "/healthz/extra", 404),
        ] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), code, "{method} {path}");
            if method == "GET" && path == "/healthz" {
                assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
                let body = to_bytes(response.into_body(), 4096).await.unwrap();
                let actual: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let expected: serde_json::Value = serde_json::from_str(include_str!(
                    "../../../protocol/fixtures/scaffold-status.json"
                ))
                .unwrap();
                assert_eq!(actual, expected);
            }
        }
    }
}
