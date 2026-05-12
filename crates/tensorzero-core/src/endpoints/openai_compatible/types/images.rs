//! Types for the OpenAI-compatible image generation relay endpoint.

use std::collections::HashMap;

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::endpoints::inference::InferenceCredentials;
use crate::error::{Error, ErrorDetails};

const TENSORZERO_MODEL_NAME_PREFIX: &str = "tensorzero::model_name::";

#[derive(Debug, Deserialize, Serialize)]
pub struct OpenAICompatibleImageGenerationParams {
    #[serde(default)]
    pub model: Option<String>,
    pub prompt: Value,
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub style: Option<String>,
    #[serde(default)]
    pub response_format: Option<String>,
    #[serde(
        default,
        rename = "tensorzero::credentials",
        serialize_with = "serialize_inference_credentials"
    )]
    pub tensorzero_credentials: InferenceCredentials,
    #[serde(default, rename = "tensorzero::include_raw_response")]
    pub tensorzero_include_raw_response: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn serialize_inference_credentials<S>(
    credentials: &InferenceCredentials,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    credentials
        .iter()
        .map(|(key, value)| (key, value.expose_secret()))
        .collect::<HashMap<_, _>>()
        .serialize(serializer)
}

impl OpenAICompatibleImageGenerationParams {
    pub fn provider_model(&self) -> Option<String> {
        self.model.as_ref().map(|model| {
            model
                .strip_prefix(TENSORZERO_MODEL_NAME_PREFIX)
                .unwrap_or(model)
                .to_string()
        })
    }

    pub fn provider_api_base(&self) -> Result<&SecretString, Error> {
        self.tensorzero_credentials
            .get("provider_api_base")
            .ok_or_else(|| {
                Error::new(ErrorDetails::InvalidOpenAICompatibleRequest {
                    message:
                        "images/generations requires tensorzero::credentials.provider_api_base"
                            .to_string(),
                })
            })
    }

    pub fn provider_type(&self) -> String {
        self.tensorzero_credentials
            .get("provider_type")
            .map(|value| value.expose_secret().trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                self.extra
                    .get("tensorzero::provider_type")
                    .and_then(Value::as_str)
                    .map(|value| value.trim().to_ascii_lowercase())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "openai".to_string())
    }

    pub fn image_provider_family(&self) -> Option<String> {
        self.tensorzero_credentials
            .get("image_provider_family")
            .or_else(|| self.tensorzero_credentials.get("imageProviderFamily"))
            .map(|value| value.expose_secret().trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
    }

    pub fn image_endpoint_family(&self) -> Option<String> {
        self.tensorzero_credentials
            .get("image_endpoint_family")
            .or_else(|| self.tensorzero_credentials.get("imageEndpointFamily"))
            .or_else(|| self.tensorzero_credentials.get("endpoint_family"))
            .or_else(|| self.tensorzero_credentials.get("endpointFamily"))
            .map(|value| value.expose_secret().trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
    }

    pub fn image_call_mode(&self) -> Option<String> {
        self.tensorzero_credentials
            .get("image_call_mode")
            .or_else(|| self.tensorzero_credentials.get("imageCallMode"))
            .or_else(|| self.tensorzero_credentials.get("call_mode"))
            .or_else(|| self.tensorzero_credentials.get("callMode"))
            .map(|value| value.expose_secret().trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
    }

    pub fn image_generation_url(&self) -> Option<&SecretString> {
        self.tensorzero_credentials
            .get("image_generation_url")
            .or_else(|| self.tensorzero_credentials.get("imageGenerationUrl"))
    }

    pub fn image_task_url_template(&self) -> Option<&SecretString> {
        self.tensorzero_credentials
            .get("image_task_url_template")
            .or_else(|| self.tensorzero_credentials.get("imageTaskUrlTemplate"))
    }

    pub fn provider_api_key(&self) -> Result<&SecretString, Error> {
        self.tensorzero_credentials
            .get("openai_api_key")
            .or_else(|| self.tensorzero_credentials.get("api_key"))
            .or_else(|| self.tensorzero_credentials.get("apiKey"))
            .ok_or_else(|| {
                Error::new(ErrorDetails::ApiKeyMissing {
                    provider_name: "openai_compatible_image".to_string(),
                    message: "images/generations requires a dynamic openai_api_key credential"
                        .to_string(),
                })
            })
    }

    pub fn provider_request_body(&self) -> Value {
        let mut body = self.extra.clone();
        body.retain(|key, _| !key.starts_with("tensorzero::"));
        if let Some(model) = self.provider_model() {
            body.insert("model".to_string(), Value::String(model));
        }
        body.insert("prompt".to_string(), self.prompt.clone());
        insert_optional(&mut body, "n", self.n.map(Value::from));
        insert_optional(&mut body, "size", self.size.clone().map(Value::String));
        insert_optional(
            &mut body,
            "quality",
            self.quality.clone().map(Value::String),
        );
        insert_optional(&mut body, "style", self.style.clone().map(Value::String));
        insert_optional(
            &mut body,
            "response_format",
            self.response_format.clone().map(Value::String),
        );
        Value::Object(body)
    }
}

fn insert_optional(body: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        body.insert(key.to_string(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_tensorzero_model_prefix_and_removes_internal_fields() {
        let params: OpenAICompatibleImageGenerationParams =
            serde_json::from_value(serde_json::json!({
                "model": "tensorzero::model_name::wan2.7-image",
                "prompt": "a shrimp factory",
                "size": "1024x1024",
                "tensorzero::include_raw_response": true,
                "tensorzero::credentials": {
                    "openai_api_key": "secret",
                    "provider_api_base": "https://example.com/v1"
                },
                "user": "lp"
            }))
            .unwrap();

        let body = params.provider_request_body();
        assert_eq!(body["model"], "wan2.7-image");
        assert_eq!(body["prompt"], "a shrimp factory");
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["user"], "lp");
        assert!(body.get("tensorzero::credentials").is_none());
        assert!(body.get("tensorzero::include_raw_response").is_none());
    }

    #[test]
    fn model_is_optional_for_provider_defaults() {
        let params: OpenAICompatibleImageGenerationParams =
            serde_json::from_value(serde_json::json!({
                "prompt": "a shrimp factory",
                "tensorzero::credentials": {
                    "openai_api_key": "secret",
                    "provider_api_base": "https://example.com/v1"
                }
            }))
            .unwrap();

        let body = params.provider_request_body();
        assert!(body.get("model").is_none());
        assert_eq!(body["prompt"], "a shrimp factory");
    }

    #[test]
    fn accepts_openai_api_key_and_provider_api_base_credentials() {
        let params: OpenAICompatibleImageGenerationParams =
            serde_json::from_value(serde_json::json!({
                "model": "image-model",
                "prompt": "test",
                "tensorzero::credentials": {
                    "openai_api_key": "secret",
                    "provider_api_base": "https://example.com/v1"
                }
            }))
            .unwrap();

        assert_eq!(params.provider_api_key().unwrap().expose_secret(), "secret");
        assert_eq!(
            params.provider_api_base().unwrap().expose_secret(),
            "https://example.com/v1"
        );
    }

    #[test]
    fn reads_provider_hint_and_injected_image_urls() {
        let params: OpenAICompatibleImageGenerationParams =
            serde_json::from_value(serde_json::json!({
                "model": "image-model",
                "prompt": "test",
                "tensorzero::credentials": {
                    "openai_api_key": "secret",
                    "provider_type": "qwen",
                    "image_provider_family": "dashscope",
                    "image_endpoint_family": "dashscope-multimodal-generation",
                    "image_call_mode": "sync",
                    "image_generation_url": "https://dashscope.aliyuncs.com/api/v1/services/aigc/text2image/image-synthesis",
                    "image_task_url_template": "https://dashscope.aliyuncs.com/api/v1/tasks/{task_id}"
                }
            }))
            .unwrap();

        assert_eq!(params.provider_type(), "qwen");
        assert_eq!(params.image_provider_family().as_deref(), Some("dashscope"));
        assert_eq!(
            params.image_endpoint_family().as_deref(),
            Some("dashscope-multimodal-generation")
        );
        assert_eq!(params.image_call_mode().as_deref(), Some("sync"));
        assert_eq!(
            params.image_generation_url().unwrap().expose_secret(),
            "https://dashscope.aliyuncs.com/api/v1/services/aigc/text2image/image-synthesis"
        );
        assert_eq!(
            params.image_task_url_template().unwrap().expose_secret(),
            "https://dashscope.aliyuncs.com/api/v1/tasks/{task_id}"
        );
    }

    #[test]
    fn requires_provider_api_base_credential() {
        let params: OpenAICompatibleImageGenerationParams =
            serde_json::from_value(serde_json::json!({
                "model": "image-model",
                "prompt": "test",
                "tensorzero::credentials": {
                    "openai_api_key": "secret"
                }
            }))
            .unwrap();

        let error = params.provider_api_base().unwrap_err();
        match error.get_details() {
            ErrorDetails::InvalidOpenAICompatibleRequest { message } => {
                assert!(message.contains("provider_api_base"));
            }
            _ => panic!("Expected InvalidOpenAICompatibleRequest error"),
        }
    }
}
