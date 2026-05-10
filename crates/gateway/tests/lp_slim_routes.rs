#![cfg(not(feature = "full-gateway"))]
#![expect(clippy::unwrap_used)]

use reqwest::{Client, StatusCode};

use crate::common::start_gateway_with_cli_bind_address;

mod common;

#[tokio::test]
async fn slim_gateway_serves_lp_routes_and_hides_full_product_routes() {
    let child_data =
        start_gateway_with_cli_bind_address(None, "127.0.0.1:0", "observability.enabled = false")
            .await;
    let client = Client::new();
    let base_url = format!("http://{}", child_data.addr);

    for path in ["/health", "/status", "/metrics"] {
        let response = client
            .get(format!("{base_url}{path}"))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{path} should remain available in the slim gateway"
        );
    }

    for path in [
        "/openai/v1/chat/completions",
        "/openai/v1/embeddings",
        "/openai/v1/images/generations",
        "/feedback",
    ] {
        let response = client
            .post(format!("{base_url}{path}"))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_ne!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} should be registered in the slim gateway"
        );
    }

    for (method, path) in [
        ("POST", "/inference"),
        ("POST", "/batch_inference"),
        ("POST", "/experimental_optimization_workflow"),
        ("POST", "/workflow_evaluation_run"),
        ("POST", "/v1/datasets/test/list_datapoints"),
        ("POST", "/v1/inferences/list_inferences"),
        ("GET", "/internal/autopilot/status"),
        ("GET", "/internal/ui_config"),
    ] {
        let request = match method {
            "GET" => client.get(format!("{base_url}{path}")),
            "POST" => client
                .post(format!("{base_url}{path}"))
                .json(&serde_json::json!({})),
            _ => unreachable!(),
        };
        let response = request.send().await.unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{method} {path} should not be registered in the slim gateway; body: {body}"
        );
    }
}
