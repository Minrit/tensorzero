//! Provider-aware image generation stateless relay.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

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

const DASH_SCOPE_ASYNC_HEADER: &str = "X-DashScope-Async";
const DASH_SCOPE_MAX_POLLS: usize = 60;
const DASH_SCOPE_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
    match image_provider_family(params) {
        ImageProviderFamily::OpenAI => relay_openai_image_generation(http_client, params).await,
        ImageProviderFamily::DashScope => {
            relay_dashscope_image_generation(http_client, params).await
        }
        ImageProviderFamily::Gemini => relay_gemini_image_generation(http_client, params).await,
        ImageProviderFamily::Unsupported(provider_type) => {
            Err(Error::new(ErrorDetails::InvalidOpenAICompatibleRequest {
                message: format!(
                    "images/generations does not support provider_type '{provider_type}'"
                ),
            }))
        }
    }
}

async fn relay_openai_image_generation(
    http_client: &crate::http::TensorzeroHttpClient,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let provider_type = params.provider_type();
    let request_url = match params.image_generation_url() {
        Some(url) => provider_url(url.expose_secret())?,
        None => image_generations_url(params.provider_api_base()?.expose_secret())?,
    };
    let api_key = params.provider_api_key()?;
    let request_body = params.provider_request_body();
    let raw_response = post_json_bearer(
        http_client,
        request_url,
        api_key.expose_secret(),
        &request_body,
        &provider_type,
    )
    .await?;

    let mut response_json = parse_provider_json(
        &raw_response.raw_request,
        &raw_response.raw_response,
        &provider_type,
    )?;
    insert_raw_response_if_requested(
        &mut response_json,
        params.tensorzero_include_raw_response,
        provider_type,
        raw_response.raw_response,
    )?;
    Ok(response_json)
}

async fn relay_dashscope_image_generation(
    http_client: &crate::http::TensorzeroHttpClient,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let provider_type = params.provider_type();
    let request_url = required_image_generation_url(params, &provider_type)?;
    let api_key = params.provider_api_key()?;
    let request_body = dashscope_request_body(params, request_url.path());
    let is_async = dashscope_requires_async(params.provider_model().as_deref());

    let mut request = http_client
        .post(request_url)
        .bearer_auth(api_key.expose_secret())
        .json(&request_body);
    if is_async {
        request = request.header(DASH_SCOPE_ASYNC_HEADER, "enable");
    }
    let raw_response =
        send_provider_request(request, &request_body, &provider_type, "image generation").await?;
    let response_json = parse_provider_json(
        &raw_response.raw_request,
        &raw_response.raw_response,
        &provider_type,
    )?;

    let normalized = if is_async {
        let task_id = response_json
            .pointer("/output/task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                provider_parse_error(
                    &raw_response,
                    &provider_type,
                    "DashScope async response has no output.task_id",
                )
            })?;
        let task_template = params.image_task_url_template().ok_or_else(|| {
            Error::new(ErrorDetails::InvalidOpenAICompatibleRequest {
                message: "DashScope async image generation requires tensorzero::credentials.image_task_url_template".to_string(),
            })
        })?;
        let task_url = dashscope_task_url(task_template.expose_secret(), task_id)?;
        let task_response = poll_dashscope_task(
            http_client,
            task_url,
            api_key.expose_secret(),
            &provider_type,
        )
        .await?;
        normalize_dashscope_response(&task_response.raw_response_json, params)?
    } else {
        normalize_dashscope_response(&response_json, params)?
    };

    with_raw_response(
        normalized,
        params.tensorzero_include_raw_response,
        provider_type,
        raw_response.raw_response,
    )
}

async fn relay_gemini_image_generation(
    http_client: &crate::http::TensorzeroHttpClient,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let provider_type = params.provider_type();
    let request_url = required_image_generation_url(params, &provider_type)?;
    let api_key = params.provider_api_key()?;
    let model = params.provider_model().unwrap_or_default();
    let request_body = if is_imagen_model(&model) {
        gemini_imagen_request_body(params)
    } else {
        gemini_generate_content_request_body(params)
    };

    let raw_response = post_json_api_key(
        http_client,
        request_url,
        api_key.expose_secret(),
        &request_body,
        &provider_type,
    )
    .await?;
    let response_json = parse_provider_json(
        &raw_response.raw_request,
        &raw_response.raw_response,
        &provider_type,
    )?;
    let normalized = if is_imagen_model(&model) {
        normalize_imagen_response(&response_json, params)?
    } else {
        normalize_gemini_generate_content_response(&response_json, params)?
    };
    with_raw_response(
        normalized,
        params.tensorzero_include_raw_response,
        provider_type,
        raw_response.raw_response,
    )
}

async fn post_json_bearer(
    http_client: &crate::http::TensorzeroHttpClient,
    request_url: Url,
    api_key: &str,
    request_body: &Value,
    provider_type: &str,
) -> Result<ProviderRawResponse, Error> {
    let request = http_client
        .post(request_url)
        .bearer_auth(api_key)
        .json(request_body);
    send_provider_request(request, request_body, provider_type, "image generation").await
}

async fn post_json_api_key(
    http_client: &crate::http::TensorzeroHttpClient,
    request_url: Url,
    api_key: &str,
    request_body: &Value,
    provider_type: &str,
) -> Result<ProviderRawResponse, Error> {
    let request = http_client
        .post(request_url)
        .header("x-goog-api-key", api_key)
        .json(request_body);
    send_provider_request(request, request_body, provider_type, "image generation").await
}

async fn send_provider_request(
    request: crate::http::TensorzeroRequestBuilder<'_>,
    request_body: &Value,
    provider_type: &str,
    operation: &str,
) -> Result<ProviderRawResponse, Error> {
    let raw_request = serde_json::to_string(&request_body).map_err(|e| {
        Error::new(ErrorDetails::Serialization {
            message: format!("Error serializing image generation request: {e}"),
        })
    })?;

    let response = request.send().await.map_err(|e| {
        Error::new(ErrorDetails::InferenceServer {
            message: format!(
                "Error sending {operation} request: {}",
                DisplayOrDebugGateway::new(e)
            ),
            raw_request: Some(raw_request.clone()),
            raw_response: None,
            provider_type: provider_type.to_string(),
            api_type: ApiType::Images,
        })
    })?;

    let status = response.status();
    let request_id = extract_request_id(response.headers());
    let raw_response = response.text().await.map_err(|e| {
        Error::new(ErrorDetails::InferenceServer {
            message: format!(
                "Error reading {operation} response: {}",
                DisplayOrDebugGateway::new(e)
            ),
            raw_request: Some(raw_request.clone()),
            raw_response: None,
            provider_type: provider_type.to_string(),
            api_type: ApiType::Images,
        })
    })?;

    if !status.is_success() {
        return Err(provider_image_error(
            status,
            &raw_request,
            &raw_response,
            request_id.as_deref(),
            provider_type,
        ));
    }

    Ok(ProviderRawResponse {
        raw_request,
        raw_response,
    })
}

fn parse_provider_json(
    raw_request: &str,
    raw_response: &str,
    provider_type: &str,
) -> Result<Value, Error> {
    serde_json::from_str(raw_response).map_err(|e| {
        Error::new(ErrorDetails::InferenceServer {
            message: format!(
                "Error parsing image generation JSON response: {}",
                DisplayOrDebugGateway::new(e)
            ),
            raw_request: Some(raw_request.to_string()),
            raw_response: Some(raw_response.to_string()),
            provider_type: provider_type.to_string(),
            api_type: ApiType::Images,
        })
    })
}

fn insert_raw_response_if_requested(
    response_json: &mut Value,
    include_raw_response: bool,
    provider_type: impl Into<String>,
    raw_response: String,
) -> Result<(), Error> {
    if !include_raw_response {
        return Ok(());
    }
    if let Some(object) = response_json.as_object_mut() {
        object.insert(
            "tensorzero::raw_response".to_string(),
            serde_json::to_value(vec![RawResponseEntry {
                model_inference_id: None,
                provider_type: provider_type.into(),
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
    Ok(())
}

fn with_raw_response(
    mut response_json: Value,
    include_raw_response: bool,
    provider_type: impl Into<String>,
    raw_response: String,
) -> Result<Value, Error> {
    insert_raw_response_if_requested(
        &mut response_json,
        include_raw_response,
        provider_type,
        raw_response,
    )?;
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

fn provider_url(raw_url: &str) -> Result<Url, Error> {
    let url = Url::parse(raw_url).map_err(|e| {
        Error::new(ErrorDetails::InvalidBaseUrl {
            message: e.to_string(),
        })
    })?;
    validate_provider_api_base(&url)?;
    Ok(url)
}

fn required_image_generation_url(
    params: &OpenAICompatibleImageGenerationParams,
    provider_type: &str,
) -> Result<Url, Error> {
    let Some(url) = params.image_generation_url() else {
        return Err(Error::new(ErrorDetails::InvalidOpenAICompatibleRequest {
            message: format!(
                "provider_type '{provider_type}' requires tensorzero::credentials.image_generation_url"
            ),
        }));
    };
    provider_url(url.expose_secret())
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

fn provider_image_error(
    status: StatusCode,
    raw_request: &str,
    raw_response: &str,
    request_id: Option<&str>,
    provider_type: &str,
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
            provider_type: provider_type.to_string(),
            api_type: ApiType::Images,
        }),
        _ => Error::new(ErrorDetails::InferenceServer {
            message,
            raw_request: Some(raw_request.to_string()),
            raw_response: Some(raw_response.to_string()),
            provider_type: provider_type.to_string(),
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

#[derive(Debug)]
struct ProviderRawResponse {
    raw_request: String,
    raw_response: String,
}

#[derive(Debug)]
struct ProviderJsonResponse {
    raw_response_json: Value,
}

#[derive(Debug)]
enum ImageProviderFamily {
    OpenAI,
    DashScope,
    Gemini,
    Unsupported(String),
}

fn image_provider_family(params: &OpenAICompatibleImageGenerationParams) -> ImageProviderFamily {
    if let Some(family) = params.image_provider_family() {
        return match family.as_str() {
            "openai" => ImageProviderFamily::OpenAI,
            "dashscope" => ImageProviderFamily::DashScope,
            "gemini" => ImageProviderFamily::Gemini,
            other => ImageProviderFamily::Unsupported(other.to_string()),
        };
    }
    match params.provider_type().as_str() {
        "openai" | "gpt" | "azure_openai" | "custom-openai" | "openai_compatible_image" => {
            ImageProviderFamily::OpenAI
        }
        "qwen" | "dashscope" | "dashscope-token-plan" => ImageProviderFamily::DashScope,
        "google" | "gemini" | "google_ai_studio_gemini" => ImageProviderFamily::Gemini,
        other => ImageProviderFamily::Unsupported(other.to_string()),
    }
}

fn dashscope_request_body(
    params: &OpenAICompatibleImageGenerationParams,
    endpoint_path: &str,
) -> Value {
    let prompt = prompt_as_string(&params.prompt);
    let size = params
        .size
        .as_deref()
        .unwrap_or("1024x1024")
        .replace('x', "*");
    let parameters = serde_json::json!({
        "size": size,
        "n": params.n.unwrap_or(1),
    });
    if endpoint_path.contains("/text2image/image-synthesis") {
        serde_json::json!({
            "model": params.provider_model(),
            "input": {
                "prompt": prompt,
            },
            "parameters": parameters,
        })
    } else {
        serde_json::json!({
            "model": params.provider_model(),
            "input": {
                "messages": [{
                    "role": "user",
                    "content": [{ "text": prompt }]
                }]
            },
            "parameters": parameters,
        })
    }
}

fn dashscope_requires_async(model: Option<&str>) -> bool {
    model
        .map(|model| {
            let model = model.to_ascii_lowercase();
            model.starts_with("wan2.7") || model.contains("wan2.7-image-pro")
        })
        .unwrap_or(false)
}

fn dashscope_task_url(template: &str, task_id: &str) -> Result<Url, Error> {
    let url = if template.contains("{task_id}") {
        template.replace("{task_id}", task_id)
    } else {
        format!("{}/{}", template.trim_end_matches('/'), task_id)
    };
    provider_url(&url)
}

async fn poll_dashscope_task(
    http_client: &crate::http::TensorzeroHttpClient,
    task_url: Url,
    api_key: &str,
    provider_type: &str,
) -> Result<ProviderJsonResponse, Error> {
    for _ in 0..DASH_SCOPE_MAX_POLLS {
        let response = http_client
            .get(task_url.clone())
            .bearer_auth(api_key)
            .send()
            .await
            .map_err(|e| {
                Error::new(ErrorDetails::InferenceServer {
                    message: format!(
                        "Error polling DashScope image generation task: {}",
                        DisplayOrDebugGateway::new(e)
                    ),
                    raw_request: None,
                    raw_response: None,
                    provider_type: provider_type.to_string(),
                    api_type: ApiType::Images,
                })
            })?;
        let status = response.status();
        let request_id = extract_request_id(response.headers());
        let raw_response = response.text().await.map_err(|e| {
            Error::new(ErrorDetails::InferenceServer {
                message: format!(
                    "Error reading DashScope task response: {}",
                    DisplayOrDebugGateway::new(e)
                ),
                raw_request: None,
                raw_response: None,
                provider_type: provider_type.to_string(),
                api_type: ApiType::Images,
            })
        })?;
        if !status.is_success() {
            return Err(provider_image_error(
                status,
                "",
                &raw_response,
                request_id.as_deref(),
                provider_type,
            ));
        }
        let response_json = parse_provider_json("", &raw_response, provider_type)?;
        match response_json
            .pointer("/output/task_status")
            .and_then(Value::as_str)
        {
            Some("SUCCEEDED") => {
                return Ok(ProviderJsonResponse {
                    raw_response_json: response_json,
                });
            }
            Some("FAILED") | Some("CANCELED") | Some("UNKNOWN") => {
                return Err(dashscope_task_error(
                    &response_json,
                    raw_response,
                    provider_type,
                ));
            }
            _ => tokio::time::sleep(DASH_SCOPE_POLL_INTERVAL).await,
        }
    }
    Err(Error::new(ErrorDetails::InferenceServer {
        message: "DashScope image generation task timed out".to_string(),
        raw_request: None,
        raw_response: None,
        provider_type: provider_type.to_string(),
        api_type: ApiType::Images,
    }))
}

fn normalize_dashscope_response(
    response_json: &Value,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let mut data = Vec::new();
    collect_dashscope_image_outputs(response_json, &mut data);
    if data.is_empty() {
        return Err(normalization_error(
            "DashScope image response has no image output",
            response_json,
            &params.provider_type(),
        ));
    }
    Ok(openai_image_response(data))
}

fn gemini_generate_content_request_body(params: &OpenAICompatibleImageGenerationParams) -> Value {
    serde_json::json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": prompt_as_string(&params.prompt) }]
        }],
        "generationConfig": {
            "responseModalities": ["IMAGE"],
            "candidateCount": params.n.unwrap_or(1),
        }
    })
}

fn gemini_imagen_request_body(params: &OpenAICompatibleImageGenerationParams) -> Value {
    serde_json::json!({
        "instances": [{ "prompt": prompt_as_string(&params.prompt) }],
        "parameters": {
            "sampleCount": params.n.unwrap_or(1),
            "aspectRatio": size_to_aspect_ratio(params.size.as_deref()),
        }
    })
}

fn normalize_gemini_generate_content_response(
    response_json: &Value,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let mut data = Vec::new();
    if let Some(parts) = response_json
        .pointer("/candidates/0/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(inline_data) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                if let Some(encoded) = inline_data.get("data").and_then(Value::as_str) {
                    let mime_type = inline_data
                        .get("mimeType")
                        .or_else(|| inline_data.get("mime_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("image/png");
                    data.push(serde_json::json!({
                        "b64_json": encoded,
                        "mime_type": mime_type,
                    }));
                }
            }
        }
    }
    if data.is_empty() {
        return Err(normalization_error(
            "Gemini image response has no inline image output",
            response_json,
            &params.provider_type(),
        ));
    }
    Ok(openai_image_response(data))
}

fn normalize_imagen_response(
    response_json: &Value,
    params: &OpenAICompatibleImageGenerationParams,
) -> Result<Value, Error> {
    let mut data = Vec::new();
    if let Some(predictions) = response_json.get("predictions").and_then(Value::as_array) {
        for prediction in predictions {
            if let Some(encoded) = prediction
                .get("bytesBase64Encoded")
                .or_else(|| prediction.get("bytes_base64_encoded"))
                .and_then(Value::as_str)
            {
                data.push(serde_json::json!({
                    "b64_json": encoded,
                    "mime_type": prediction
                        .get("mimeType")
                        .or_else(|| prediction.get("mime_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("image/png"),
                }));
            }
        }
    }
    if data.is_empty() {
        return Err(normalization_error(
            "Imagen response has no base64 image output",
            response_json,
            &params.provider_type(),
        ));
    }
    Ok(openai_image_response(data))
}

fn openai_image_response(data: Vec<Value>) -> Value {
    serde_json::json!({
        "created": chrono::Utc::now().timestamp(),
        "data": data,
    })
}

fn collect_dashscope_image_outputs(response_json: &Value, data: &mut Vec<Value>) {
    for pointer in ["/output/results", "/output/choices", "/results", "/data"] {
        if let Some(values) = response_json.pointer(pointer).and_then(Value::as_array) {
            collect_dashscope_image_array(values, data);
        }
    }
}

fn collect_dashscope_image_array(values: &[Value], data: &mut Vec<Value>) {
    for value in values {
        let Some(object) = value.as_object() else {
            continue;
        };
        if let Some(encoded) = object
            .get("b64_json")
            .or_else(|| object.get("base64"))
            .or_else(|| object.get("image_base64"))
            .and_then(Value::as_str)
        {
            data.push(serde_json::json!({ "b64_json": encoded }));
            continue;
        }
        if let Some(url) = object
            .get("url")
            .or_else(|| object.get("image_url"))
            .or_else(|| object.get("imageUrl"))
            .and_then(Value::as_str)
        {
            data.push(serde_json::json!({ "url": url }));
        }
    }
}

fn dashscope_task_error(response_json: &Value, raw_response: String, provider_type: &str) -> Error {
    let code = response_json
        .pointer("/output/code")
        .or_else(|| response_json.get("code"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let message = response_json
        .pointer("/output/message")
        .or_else(|| response_json.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(&raw_response)
        .to_string();
    let client_error = matches!(
        code,
        "InvalidParameter"
            | "DataInspectionFailed"
            | "InvalidApiKey"
            | "AccessDenied"
            | "QuotaExceeded"
            | "Throttling"
    );
    if client_error {
        Error::new(ErrorDetails::InferenceClient {
            status_code: Some(StatusCode::BAD_REQUEST),
            message,
            raw_request: None,
            raw_response: Some(response_json.to_string()),
            provider_type: provider_type.to_string(),
            api_type: ApiType::Images,
        })
    } else {
        Error::new(ErrorDetails::InferenceServer {
            message,
            raw_request: None,
            raw_response: Some(response_json.to_string()),
            provider_type: provider_type.to_string(),
            api_type: ApiType::Images,
        })
    }
}

fn provider_parse_error(
    raw_response: &ProviderRawResponse,
    provider_type: &str,
    message: &str,
) -> Error {
    Error::new(ErrorDetails::InferenceServer {
        message: message.to_string(),
        raw_request: Some(raw_response.raw_request.clone()),
        raw_response: Some(raw_response.raw_response.clone()),
        provider_type: provider_type.to_string(),
        api_type: ApiType::Images,
    })
}

fn normalization_error(message: &str, response_json: &Value, provider_type: &str) -> Error {
    Error::new(ErrorDetails::InferenceServer {
        message: message.to_string(),
        raw_request: None,
        raw_response: Some(response_json.to_string()),
        provider_type: provider_type.to_string(),
        api_type: ApiType::Images,
    })
}

fn prompt_as_string(prompt: &Value) -> String {
    prompt
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| prompt.to_string())
}

fn is_imagen_model(model: &str) -> bool {
    model.to_ascii_lowercase().contains("imagen")
}

fn size_to_aspect_ratio(size: Option<&str>) -> &'static str {
    match size {
        Some("1024x1792") => "9:16",
        Some("1792x1024") => "16:9",
        _ => "1:1",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image_params(provider_type: &str, family: &str) -> OpenAICompatibleImageGenerationParams {
        serde_json::from_value(serde_json::json!({
            "model": "qwen-image-2.0-pro",
            "prompt": "a shrimp factory",
            "n": 1,
            "size": "1024x1024",
            "tensorzero::credentials": {
                "openai_api_key": "secret",
                "provider_type": provider_type,
                "image_provider_family": family,
                "image_generation_url": "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation"
            }
        }))
        .unwrap()
    }

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
        let error = provider_image_error(
            StatusCode::TOO_MANY_REQUESTS,
            "{}",
            "{\"error\":\"limited\"}",
            Some("req_123"),
            "openai",
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

    #[test]
    fn image_provider_family_prefers_lp_injected_family() {
        let params = image_params("new-dashscope-alias", "dashscope");
        assert!(matches!(
            image_provider_family(&params),
            ImageProviderFamily::DashScope
        ));
    }

    #[test]
    fn dashscope_multimodal_request_uses_messages_shape() {
        let params = image_params("qwen", "dashscope");
        let body = dashscope_request_body(
            &params,
            "/api/v1/services/aigc/multimodal-generation/generation",
        );

        assert_eq!(body["model"], "qwen-image-2.0-pro");
        assert_eq!(
            body["input"]["messages"][0]["content"][0]["text"],
            "a shrimp factory"
        );
        assert!(body["input"].get("prompt").is_none());
        assert_eq!(body["parameters"]["size"], "1024*1024");
    }

    #[test]
    fn dashscope_text2image_request_keeps_prompt_shape() {
        let params = image_params("qwen", "dashscope");
        let body =
            dashscope_request_body(&params, "/api/v1/services/aigc/text2image/image-synthesis");

        assert_eq!(body["input"]["prompt"], "a shrimp factory");
        assert!(body["input"].get("messages").is_none());
    }

    #[test]
    fn dashscope_normalization_reads_only_documented_output_arrays() {
        let params = image_params("qwen", "dashscope");
        let response = serde_json::json!({
            "request": { "url": "https://example.com/not-an-image" },
            "output": {
                "results": [{ "url": "https://cdn.example.com/image.png" }]
            }
        });

        let normalized = normalize_dashscope_response(&response, &params).unwrap();
        assert_eq!(
            normalized["data"][0]["url"],
            "https://cdn.example.com/image.png"
        );
    }

    #[test]
    fn dashscope_normalization_rejects_unrelated_url_fields() {
        let params = image_params("qwen", "dashscope");
        let response = serde_json::json!({
            "request": { "url": "https://example.com/not-an-image" },
            "metadata": { "imageUrl": "https://example.com/also-not-image-output" }
        });

        let error = normalize_dashscope_response(&response, &params).unwrap_err();
        match error.get_details() {
            ErrorDetails::InferenceServer { message, .. } => {
                assert!(message.contains("no image output"));
            }
            _ => panic!("Expected InferenceServer normalization error"),
        }
    }

    #[test]
    fn gemini_and_imagen_normalizers_return_openai_image_data() {
        let params = image_params("google", "gemini");
        let gemini = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "inlineData": {
                            "mimeType": "image/png",
                            "data": "abc"
                        }
                    }]
                }
            }]
        });
        let imagen = serde_json::json!({
            "predictions": [{
                "mimeType": "image/png",
                "bytesBase64Encoded": "def"
            }]
        });

        assert_eq!(
            normalize_gemini_generate_content_response(&gemini, &params).unwrap()["data"][0]["b64_json"],
            "abc"
        );
        assert_eq!(
            normalize_imagen_response(&imagen, &params).unwrap()["data"][0]["b64_json"],
            "def"
        );
    }

    #[test]
    fn dashscope_task_invalid_parameter_is_client_error() {
        let error = dashscope_task_error(
            &serde_json::json!({
                "output": {
                    "task_status": "FAILED",
                    "code": "InvalidParameter",
                    "message": "num_images_per_prompt must be 1"
                }
            }),
            "{}".to_string(),
            "dashscope",
        );

        assert!(matches!(
            error.get_details(),
            ErrorDetails::InferenceClient { .. }
        ));
    }
}
