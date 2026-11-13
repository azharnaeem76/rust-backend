mod common;
mod items;
mod stats;

use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::Request,
    http::{header, HeaderValue, Method},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use tower_http::cors::CorsLayer;

use crate::common::ApiError;

#[tokio::main]
async fn main() {
    let port = env::var("PORT").unwrap_or_else(|_| "4001".to_owned());
    let data_path = env::var("DATA_PATH").unwrap_or_else(|_| "../data/items.json".to_owned());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .expect("failed to bind port");

    println!("Backend running on http://localhost:{port}");

    axum::serve(listener, router(PathBuf::from(data_path)))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    println!("Server stopped.");
}

/// Builds the full application: routes, request logging and CORS.
fn router(data_path: PathBuf) -> Router {
    let items_router = Router::new()
        .route("/api/items", get(items::list).post(items::create))
        .route(
            "/api/items/:id",
            get(items::get_one).put(items::update).delete(items::delete),
        )
        .with_state(Arc::new(items::Handler::new(data_path.clone())));

    let stats_router = Router::new()
        .route("/api/stats", get(stats::get_stats))
        .with_state(Arc::new(stats::Handler::new(data_path)));

    let cors = CorsLayer::new()
        .allow_origin([
            HeaderValue::from_static("http://localhost:3000"),
            HeaderValue::from_static("http://localhost:4001"),
        ])
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE]);

    Router::new()
        .merge(items_router)
        .merge(stats_router)
        // Must come after the routes are added: it applies to the routes present now.
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(route_not_found)
        .layer(middleware::from_fn(log_request))
        .layer(cors)
}

/// Unknown paths get the same `{ "error": ... }` body as every other error.
async fn route_not_found() -> ApiError {
    ApiError::NotFound("route not found".into())
}

/// A known path with an unsupported method. axum still adds the `Allow` header.
async fn method_not_allowed() -> ApiError {
    ApiError::MethodNotAllowed
}

/// Logs method, path, status and duration for every request.
async fn log_request(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    let start = Instant::now();

    let response = next.run(req).await;

    println!(
        "{method} {path} {} {:?}",
        response.status().as_u16(),
        start.elapsed()
    );
    response
}

/// Waits for Ctrl+C or SIGTERM, then lets `axum::serve` drain in-flight requests.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    println!("Shutting down gracefully...");
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request as HttpRequest, StatusCode};
    use serde_json::{json, Value};
    use tower::ServiceExt;

    use super::*;
    use crate::common::test_support::temp_data_file;

    const SAMPLE: &str = r#"[
        {"id": 1, "name": "Laptop Pro", "category": "Electronics", "price": 2499},
        {"id": 2, "name": "Standing Desk", "category": "Furniture", "price": 1199},
        {"id": 3, "name": "Laptop Stand", "category": "Accessories", "price": 49}
    ]"#;

    /// Sends one request and returns the status plus the JSON body (`Null` if empty).
    async fn send(
        app: &Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let builder = HttpRequest::builder().method(method).uri(uri);
        let request = match body {
            Some(json) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json.to_string())),
            None => builder.body(Body::empty()),
        }
        .unwrap();

        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn list_filters_and_paginates() {
        let app = router(temp_data_file(SAMPLE));

        let (status, body) = send(&app, Method::GET, "/api/items?q=LAPTOP", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["total"], 2);

        let (_, body) = send(&app, Method::GET, "/api/items?page=2&pageSize=2", None).await;
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["id"], 3);

        // Out-of-range values are clamped rather than rejected.
        let (_, body) = send(&app, Method::GET, "/api/items?page=0&pageSize=1000", None).await;
        assert_eq!(body["page"], 1);
        assert_eq!(body["pageSize"], 200);

        // A page past the end is empty but still reports the total.
        let (_, body) = send(&app, Method::GET, "/api/items?page=99", None).await;
        assert_eq!(body["items"], json!([]));
        assert_eq!(body["total"], 3);

        let (status, body) = send(&app, Method::GET, "/api/items?page=abc", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].is_string());
    }

    #[tokio::test]
    async fn get_one_handles_found_missing_and_invalid_ids() {
        let app = router(temp_data_file(SAMPLE));

        let (status, body) = send(&app, Method::GET, "/api/items/1", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "Laptop Pro");

        let (status, _) = send(&app, Method::GET, "/api/items/999", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(&app, Method::GET, "/api/items/abc", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_rejects_invalid_input_without_saving() {
        let app = router(temp_data_file(SAMPLE));

        for bad in [
            json!({ "name": "   ", "category": "Misc", "price": 10 }),
            json!({ "category": "Misc", "price": 10 }),
            json!({ "name": "Mouse", "category": "Misc", "price": -1 }),
            json!({ "name": "Mouse", "category": "Misc" }),
            json!({ "name": "Mouse", "price": "ten" }),
        ] {
            let (status, body) = send(&app, Method::POST, "/api/items", Some(bad.clone())).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "payload: {bad}");
            assert!(body["error"].is_string(), "payload: {bad}");
        }

        let (status, _) = send(&app, Method::POST, "/api/items", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "empty body");

        let (_, body) = send(&app, Method::GET, "/api/items", None).await;
        assert_eq!(body["total"], 3);
    }

    #[tokio::test]
    async fn create_saves_a_trimmed_item_with_the_next_id() {
        let app = router(temp_data_file(SAMPLE));

        let payload = json!({ "name": "  Mouse ", "category": "Accessories", "price": 25 });
        let (status, body) = send(&app, Method::POST, "/api/items", Some(payload)).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            body,
            json!({ "id": 4, "name": "Mouse", "category": "Accessories", "price": 25.0 })
        );

        let (status, _) = send(&app, Method::GET, "/api/items/4", None).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn update_changes_fields_but_not_the_id() {
        let app = router(temp_data_file(SAMPLE));

        let payload = json!({ "id": 42, "name": "Desk XL", "category": "Office", "price": 1299 });
        let (status, body) = send(&app, Method::PUT, "/api/items/2", Some(payload)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({ "id": 2, "name": "Desk XL", "category": "Office", "price": 1299.0 })
        );

        let (_, body) = send(&app, Method::GET, "/api/items/2", None).await;
        assert_eq!(body["name"], "Desk XL");

        let valid = json!({ "name": "X", "category": "Y", "price": 1 });
        let cases = [
            ("/api/items/999", Some(valid.clone()), StatusCode::NOT_FOUND),
            ("/api/items/abc", Some(valid), StatusCode::BAD_REQUEST),
            (
                "/api/items/2",
                Some(json!({ "name": "", "price": 1 })),
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/items/2",
                Some(json!({ "name": "X", "price": -5 })),
                StatusCode::BAD_REQUEST,
            ),
            ("/api/items/2", None, StatusCode::BAD_REQUEST),
        ];
        for (uri, payload, expected) in cases {
            let (status, _) = send(&app, Method::PUT, uri, payload).await;
            assert_eq!(status, expected, "PUT {uri}");
        }
    }

    #[tokio::test]
    async fn delete_removes_the_item() {
        let app = router(temp_data_file(SAMPLE));

        let (status, body) = send(&app, Method::DELETE, "/api/items/1", None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(body, Value::Null);

        let (status, _) = send(&app, Method::GET, "/api/items/1", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(&app, Method::DELETE, "/api/items/1", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = send(&app, Method::DELETE, "/api/items/abc", None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (_, body) = send(&app, Method::GET, "/api/items", None).await;
        assert_eq!(body["total"], 2);
    }

    #[tokio::test]
    async fn stats_follow_writes() {
        let app = router(temp_data_file(SAMPLE));

        let (status, body) = send(&app, Method::GET, "/api/stats", None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "total": 3, "averagePrice": 1249.0 }));

        send(&app, Method::DELETE, "/api/items/1", None).await;
        let (_, body) = send(&app, Method::GET, "/api/stats", None).await;
        assert_eq!(body, json!({ "total": 2, "averagePrice": 624.0 }));
    }

    #[tokio::test]
    async fn unknown_routes_and_methods_return_json_errors() {
        let app = router(temp_data_file(SAMPLE));

        let (status, body) = send(&app, Method::GET, "/api/nope", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].is_string());

        let (status, body) = send(&app, Method::PATCH, "/api/items/1", None).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        assert!(body["error"].is_string());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_creates_do_not_lose_updates() {
        let app = router(temp_data_file(SAMPLE));

        let tasks: Vec<_> = (0..50)
            .map(|i| {
                let app = app.clone();
                tokio::spawn(async move {
                    let payload =
                        json!({ "name": format!("Item {i}"), "category": "Bulk", "price": i });
                    send(&app, Method::POST, "/api/items", Some(payload))
                        .await
                        .0
                })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), StatusCode::CREATED);
        }

        let (_, body) = send(&app, Method::GET, "/api/items?pageSize=200", None).await;
        assert_eq!(body["total"], 53);
        let mut ids: Vec<i64> = body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_i64().unwrap())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 53, "every item kept a unique id");
    }
}
