use std::sync::atomic::Ordering;

use axum::http::StatusCode;

use super::fixture::rejecting_client;

#[tokio::test]
async fn query_does_not_retry_auth_or_payment_rejections() {
    for status in [
        StatusCode::UNAUTHORIZED,
        StatusCode::PAYMENT_REQUIRED,
        StatusCode::FORBIDDEN,
    ] {
        let (client, requests) = rejecting_client(status).await;

        let error = client.embed_query("query").await.unwrap_err();
        assert!(error.to_string().contains(status.as_str()));
        assert_eq!(requests.load(Ordering::SeqCst), 1, "{status} was retried");
    }
}

#[tokio::test]
async fn ollama_omits_authorization_without_key_and_decodes_native_embeddings() {
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Seen {
        authorization: Arc<Mutex<Option<String>>>,
        model: Arc<Mutex<Option<String>>>,
    }

    let seen = Seen::default();
    let app = Router::new().route(
        "/api/embed",
        post({
            let seen = seen.clone();
            move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
                *seen.authorization.lock().unwrap() = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .map(|value| value.to_str().unwrap_or_default().to_owned());
                *seen.model.lock().unwrap() = body
                    .get("model")
                    .and_then(|model| model.as_str())
                    .map(str::to_owned);
                assert_eq!(
                    body.get("input")
                        .and_then(|input| input.as_array())
                        .map(Vec::len),
                    Some(1)
                );
                assert!(body.get("input_type").is_none());
                assert!(body.get("dimensions").is_none());
                Json(serde_json::json!({"model": "test-model", "embeddings": [[0.5, -0.25]]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = super::super::VoyageClient::new_for_provider(
        super::super::Provider::Ollama,
        "test-model".to_string(),
        Vec::new(),
        Some(&format!("http://{addr}")),
        None,
    )
    .unwrap();
    let vectors = client
        .embed(
            &["hello".to_string()],
            crate::embedding::InputType::Document,
        )
        .await
        .unwrap();
    assert_eq!(vectors, vec![vec![0.5, -0.25]]);
    let query = client.embed_query("hello").await.unwrap();
    assert_eq!(query, vec![0.5, -0.25]);
    assert_eq!(*seen.authorization.lock().unwrap(), None);
    assert_eq!(seen.model.lock().unwrap().as_deref(), Some("test-model"));
}

#[tokio::test]
async fn ollama_sends_configured_key_as_bearer_auth() {
    use axum::{Json, Router, routing::post};
    use std::sync::{Arc, Mutex};

    let authorization = Arc::new(Mutex::new(None));
    let app = Router::new().route(
        "/api/embed",
        post({
            let authorization = authorization.clone();
            move |headers: axum::http::HeaderMap| async move {
                *authorization.lock().unwrap() = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .map(|value| value.to_str().unwrap_or_default().to_owned());
                Json(serde_json::json!({"model": "test-model", "embeddings": [[1.0]]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let client = super::super::VoyageClient::new_for_provider(
        super::super::Provider::Ollama,
        "test-model".to_string(),
        vec!["secret".to_string()],
        Some(&format!("http://{addr}")),
        None,
    )
    .unwrap();
    let vectors = client
        .embed(
            &["hello".to_string()],
            crate::embedding::InputType::Document,
        )
        .await
        .unwrap();
    assert_eq!(vectors, vec![vec![1.0]]);
    let query = client.embed_query("hello").await.unwrap();
    assert_eq!(query, vec![1.0]);
    assert_eq!(
        authorization.lock().unwrap().as_deref(),
        Some("Bearer secret")
    );
}
