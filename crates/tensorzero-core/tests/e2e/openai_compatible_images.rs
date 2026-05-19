use reqwest::{Client, StatusCode};
use serde_json::json;

use crate::common::get_gateway_endpoint;

#[tokio::test]
async fn openai_compatible_images_rejects_private_provider_api_base() {
    let client = Client::new();
    let response = client
        .post(get_gateway_endpoint("/openai/v1/images/generations"))
        .json(&json!({
            "prompt": "a shrimp factory",
            "tensorzero::credentials": {
                "openai_api_key": "test",
                "provider_api_base": "http://127.0.0.1:12345/v1"
            }
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = response.text().await.unwrap();
    assert!(
        body.contains("provider_api_base IP address is not allowed")
            || body.contains("provider_api_base host is not allowed"),
        "unexpected body: {body}"
    );
}
