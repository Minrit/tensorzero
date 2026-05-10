//! OpenAI-compatible image generation stateless relay.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use secrecy::ExposeSecret;
use serde_json::Value;
use url::Url;

use crate::error::{DisplayOrDebugGateway, Error, ErrorDetails};
use crate::inference::types::{ApiType, RawResponseEntry};
use crate::utils::gateway::{AppState, AppStateData};

use super::types::images::OpenAICompatibleImageGenerationParams;
use super::{OpenAICompatibleError, OpenAIStructuredJson};

const PROVIDER_TYPE: &str = "openai_compatible_image";

pub async fn image_generations_handler(
    State(AppStateData { http_client, .. }): AppState,
    OpenAIStructuredJson(params): OpenAIStructuredJson<OpenAICompatibleImageGenerationParams>,
) -> Result<Response, OpenAICompatibleError> {
    let include_raw_response = params.tensorzero_include_raw_response;
    match relay_image_generation(&http_client, &params).await {
        Ok(response) => Ok(Json(response).into_response()),
        Err(error) => Ok(error.into_response_with_raw_entries(true, include_raw_response)),
    }
}

async fn relay_image_generation(
    http_client: &crate::http::TensorzeroHttpClient,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let request_url = image_generations_url(params.provider_api_base()?.expose_secret())?;
    let api_key = params.provider_api_key()?;
    let request_body = params.provider_request_body();
    let raw_request = serde_json::to_string(&request_body).map_err(|e| {
        Error::new(ErrorDetails::Serialization {
            message: format!("Error serializing image generation request: {e}"),
        })
    })?;

    let response = http_client
        .post(request_url)
        .bearer_auth(api_key.expose_secret())
        .json(&request_body)
        .send()
        .await
        .map_err(|e| {
            Error::new(ErrorDetails::InferenceServer {
                message: format!(
                    "Error sending image generation request: {}",
                    DisplayOrDebugGateway::new(e)
                ),
                raw_request: Some(raw_request.clone()),
                raw_response: None,
                provider_type: PROVIDER_TYPE.to_string(),
                api_type: ApiType::Images,
            })
        })?;

    let status = response.status();
    let request_id = extract_request_id(response.headers());
    let raw_response = response.text().await.map_err(|e| {
        Error::new(ErrorDetails::InferenceServer {
            message: format!(
                "Error reading image generation response: {}",
                DisplayOrDebugGateway::new(e)
            ),
            raw_request: Some(raw_request.clone()),
            raw_response: None,
            provider_type: PROVIDER_TYPE.to_string(),
            api_type: ApiType::Images,
        })
    })?;

    if !status.is_success() {
        return Err(openai_image_error(
            status,
            &raw_request,
            &raw_response,
            request_id.as_deref(),
        ));
    }

    let mut response_json: Value = serde_json::from_str(&raw_response).map_err(|e| {
        Error::new(ErrorDetails::InferenceServer {
            message: format!(
                "Error parsing image generation JSON response: {}",
                DisplayOrDebugGateway::new(e)
            ),
            raw_request: Some(raw_request),
            raw_response: Some(raw_response.clone()),
            provider_type: PROVIDER_TYPE.to_string(),
            api_type: ApiType::Images,
        })
    })?;
    if params.tensorzero_include_raw_response {
        if let Some(object) = response_json.as_object_mut() {
            object.insert(
                "tensorzero::raw_response".to_string(),
                serde_json::to_value(vec![RawResponseEntry {
                    model_inference_id: None,
                    provider_type: PROVIDER_TYPE.to_string(),
                    api_type: ApiType::Images,
                    data: raw_response,
                }])
                .map_err(|e| {
                    Error::new(ErrorDetails::Serialization {
                        message: format!("Error serializing image generation raw response: {e}"),
                    })
                })?,
            );
        }
    }
    Ok(response_json)
}

fn image_generations_url(api_base: &str) -> Result<Url, Error> {
    let mut url = Url::parse(api_base).map_err(|e| {
        Error::new(ErrorDetails::InvalidBaseUrl {
            message: e.to_string(),
        })
    })?;
    validate_provider_api_base(&url)?;
    if !url.path().ends_with('/') {
        url.set_path(&format!("{}/", url.path()));
    }
    url.join("images/generations").map_err(|e| {
        Error::new(ErrorDetails::InvalidBaseUrl {
            message: e.to_string(),
        })
    })
}

fn validate_provider_api_base(url: &Url) -> Result<(), Error> {
    if !matches!(url.scheme(), "http" | "https") {
        return invalid_base_url("provider_api_base must use http or https");
    }
    let Some(host) = url.host_str() else {
        return invalid_base_url("provider_api_base must include a host");
    };
    let host_lower = host.to_ascii_lowercase();
    if matches!(
        host_lower.as_str(),
        "localhost"
            | "metadata.google.internal"
            | "metadata"
            | "169.254.169.254"
            | "host.docker.internal"
            | "host.containers.internal"
    ) || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
    {
        return invalid_base_url("provider_api_base host is not allowed");
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return invalid_base_url("provider_api_base IP address is not allowed");
        }
    }
    Ok(())
}

fn invalid_base_url<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error::new(ErrorDetails::InvalidBaseUrl {
        message: message.into(),
    }))
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_blocked_ipv4(ip),
        IpAddr::V6(ip) => is_blocked_ipv6(ip),
    }
}

fn is_blocked_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
}

fn is_blocked_ipv6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_unique_local()
        || ip.is_unicast_link_local()
        || ip.is_multicast()
}

fn openai_image_error(
    status: StatusCode,
    raw_request: &str,
    raw_response: &str,
    request_id: Option<&str>,
) -> Error {
    let message = match request_id {
        Some(id) => format!("{raw_response} [request_id: {id}]"),
        None => raw_response.to_string(),
    };
    match status {
        StatusCode::BAD_REQUEST
        | StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::TOO_MANY_REQUESTS => Error::new(ErrorDetails::InferenceClient {
            status_code: Some(status),
            message,
            raw_request: Some(raw_request.to_string()),
            raw_response: Some(raw_response.to_string()),
            provider_type: PROVIDER_TYPE.to_string(),
            api_type: ApiType::Images,
        }),
        _ => Error::new(ErrorDetails::InferenceServer {
            message,
            raw_request: Some(raw_request.to_string()),
            raw_response: Some(raw_response.to_string()),
            provider_type: PROVIDER_TYPE.to_string(),
            api_type: ApiType::Images,
        }),
    }
}

fn extract_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_url_appends_generations_path() {
        let url = image_generations_url("https://example.com/openai/v1").unwrap();
        assert_eq!(
            url.as_str(),
            "https://example.com/openai/v1/images/generations"
        );
    }

    #[test]
    fn image_url_rejects_private_provider_api_base() {
        let err = image_generations_url("http://127.0.0.1:8080/v1").unwrap_err();
        match err.get_details() {
            ErrorDetails::InvalidBaseUrl { message } => {
                assert!(message.contains("not allowed"));
            }
            _ => panic!("Expected InvalidBaseUrl error"),
        }
    }

    #[test]
    fn image_error_preserves_underlying_status() {
        let error = openai_image_error(
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            "{\"error\":\"limited\"}",
            Some("req_123"),
        );
        assert_eq!(
            error.underlying_status_code(),
            Some(StatusCode::TOO_MANY_REQUESTS)
        );
        let body = error.build_response_body(true, None);
        assert_eq!(
            body["error"]["tensorzero_error_json"]["tensorzero"]["apiType"],
            "images"
        );
        assert_eq!(
            body["error"]["tensorzero_error_json"]["upstream"]["statusCode"],
            429
        );
    }
}
