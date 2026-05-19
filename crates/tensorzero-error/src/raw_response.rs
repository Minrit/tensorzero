use tensorzero_types::RawResponseEntry;

use serde_json::Value;

pub(crate) const RAW_RESPONSE_TRUNCATION_BYTES: usize = 64 * 1024;

const IMAGE_FIELD_MIN_PEEL_BYTES: usize = 4 * 1024;

pub(crate) fn truncate_raw(raw: Option<String>) -> Option<String> {
    raw.map(truncate_raw_string)
}

pub(crate) fn truncate_raw_entries(entries: Vec<RawResponseEntry>) -> Vec<RawResponseEntry> {
    entries
        .into_iter()
        .map(|entry| RawResponseEntry {
            data: truncate_raw_string(entry.data),
            ..entry
        })
        .collect()
}

pub(crate) fn truncate_raw_string(raw: String) -> String {
    let peeled = peel_image_base64_fields(raw);
    truncate_to_limit(peeled)
}

fn peel_image_base64_fields(raw: String) -> String {
    if !might_contain_image_base64_key(&raw) {
        return raw;
    }

    let Ok(mut value) = serde_json::from_str::<Value>(&raw) else {
        return raw;
    };

    if !peel_image_base64_in_place(&mut value) {
        return raw;
    }

    match serde_json::to_string(&value) {
        Ok(peeled) => peeled,
        Err(_) => raw,
    }
}

fn peel_image_base64_in_place(value: &mut Value) -> bool {
    let mut changed = false;

    match value {
        Value::Array(values) => {
            for value in values {
                changed |= peel_image_base64_in_place(value);
            }
        }
        Value::Object(object) => {
            for image_key in ["inlineData", "inline_data"] {
                if let Some(Value::Object(image_object)) = object.get_mut(image_key)
                    && let Some(Value::String(data)) = image_object.get_mut("data")
                    && data.len() > IMAGE_FIELD_MIN_PEEL_BYTES
                {
                    let len = data.len();
                    *data = image_marker(len);
                    changed = true;
                }
            }

            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "b64_json"
                        | "b64"
                        | "base64"
                        | "image_base64"
                        | "bytesBase64Encoded"
                        | "bytes_base64_encoded"
                ) && let Value::String(data) = value
                    && data.len() > IMAGE_FIELD_MIN_PEEL_BYTES
                {
                    let len = data.len();
                    *data = image_marker(len);
                    changed = true;
                    continue;
                }

                changed |= peel_image_base64_in_place(value);
            }
        }
        _ => {}
    }

    changed
}

fn might_contain_image_base64_key(raw: &str) -> bool {
    [
        "b64_json",
        "\"b64\"",
        "base64",
        "image_base64",
        "bytesBase64Encoded",
        "bytes_base64_encoded",
        "inlineData",
        "inline_data",
    ]
    .into_iter()
    .any(|key| raw.contains(key))
}

fn image_marker(len: usize) -> String {
    format!("<image:{len} bytes>")
}

fn truncate_to_limit(raw: String) -> String {
    if raw.len() <= RAW_RESPONSE_TRUNCATION_BYTES {
        return raw;
    }

    let footer = format!(
        "\n...<truncated: {} bytes total, {} kept>",
        raw.len(),
        RAW_RESPONSE_TRUNCATION_BYTES
    );
    let max_prefix_len = RAW_RESPONSE_TRUNCATION_BYTES.saturating_sub(footer.len());
    let keep_len = floor_char_boundary(&raw, max_prefix_len);
    let footer = format!(
        "\n...<truncated: {} bytes total, {} kept>",
        raw.len(),
        keep_len
    );
    let keep_len = floor_char_boundary(
        &raw,
        RAW_RESPONSE_TRUNCATION_BYTES.saturating_sub(footer.len()),
    );

    let mut truncated = String::with_capacity(RAW_RESPONSE_TRUNCATION_BYTES);
    truncated.push_str(&raw[..keep_len]);
    truncated.push_str(&footer);
    debug_assert!(truncated.len() <= RAW_RESPONSE_TRUNCATION_BYTES);
    truncated
}

fn floor_char_boundary(raw: &str, mut index: usize) -> usize {
    index = index.min(raw.len());
    while !raw.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    fn large_image_data() -> String {
        "a".repeat(1024 * 1024)
    }

    #[gtest]
    fn none_stays_none() {
        expect_that!(truncate_raw(None), none());
    }

    #[gtest]
    fn raw_response_truncation_bytes_is_the_contract_limit() {
        expect_that!(RAW_RESPONSE_TRUNCATION_BYTES, eq(64 * 1024));
    }

    #[gtest]
    fn small_text_stays_unchanged() {
        let raw = "x".repeat(100);

        assert_eq!(truncate_raw(Some(raw.clone())), Some(raw));
    }

    #[gtest]
    fn small_json_stays_byte_for_byte_unchanged() {
        let raw = r#"{"error":{"message":"insufficient_quota","code":"insufficient_quota"}}"#;

        assert_eq!(truncate_raw(Some(raw.to_string())), Some(raw.to_string()));
    }

    #[gtest]
    fn exact_threshold_stays_unchanged() {
        let raw = "x".repeat(RAW_RESPONSE_TRUNCATION_BYTES);

        assert_eq!(truncate_raw(Some(raw.clone())), Some(raw));
    }

    #[gtest]
    fn one_byte_over_threshold_is_bounded() {
        let raw = "x".repeat(RAW_RESPONSE_TRUNCATION_BYTES + 1);
        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after truncation");
        };

        expect_that!(truncated.len(), le(RAW_RESPONSE_TRUNCATION_BYTES));
        expect_that!(truncated, contains_substring("<truncated:"));
    }

    #[gtest]
    fn utf8_boundary_near_truncation_point_stays_valid() {
        let raw = format!(
            "{}{}",
            "x".repeat(RAW_RESPONSE_TRUNCATION_BYTES - 10),
            "中".repeat(100)
        );
        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after truncation");
        };

        expect_that!(truncated.len(), le(RAW_RESPONSE_TRUNCATION_BYTES));
        let _: Vec<char> = truncated.chars().collect();
    }

    #[gtest]
    fn peels_b64_json_and_preserves_metadata() {
        let image = large_image_data();
        let raw = serde_json::json!({
            "data": [{ "b64_json": image }],
            "model": "gpt-image-1"
        })
        .to_string();

        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after peeling");
        };

        expect_that!(truncated.len(), le(RAW_RESPONSE_TRUNCATION_BYTES));
        let value: Value = serde_json::from_str(&truncated)
            .unwrap_or_else(|e| panic!("peeled JSON response should parse: {e}"));
        expect_that!(
            value["data"][0]["b64_json"].as_str(),
            some(eq("<image:1048576 bytes>"))
        );
        expect_that!(value["model"].as_str(), some(eq("gpt-image-1")));
    }

    #[gtest]
    fn peels_b64_json_and_preserves_provider_diagnostics() {
        let raw = serde_json::json!({
            "error": {
                "message": "image quota exceeded",
                "code": "insufficient_quota"
            },
            "request_id": "req_123",
            "data": [{ "b64_json": large_image_data() }]
        })
        .to_string();

        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after peeling");
        };

        expect_that!(truncated.len(), le(RAW_RESPONSE_TRUNCATION_BYTES));
        let value: Value = serde_json::from_str(&truncated)
            .unwrap_or_else(|e| panic!("peeled provider JSON should parse: {e}"));
        expect_that!(
            value["error"]["message"].as_str(),
            some(eq("image quota exceeded"))
        );
        expect_that!(
            value["error"]["code"].as_str(),
            some(eq("insufficient_quota"))
        );
        expect_that!(value["request_id"].as_str(), some(eq("req_123")));
        expect_that!(
            value["data"][0]["b64_json"].as_str(),
            some(eq("<image:1048576 bytes>"))
        );
    }

    #[gtest]
    fn peels_dashscope_b64_field() {
        let raw = serde_json::json!({
            "output": {
                "results": [{ "b64": large_image_data() }]
            }
        })
        .to_string();

        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after peeling");
        };

        let value: Value = serde_json::from_str(&truncated)
            .unwrap_or_else(|e| panic!("peeled DashScope JSON should parse: {e}"));
        expect_that!(
            value["output"]["results"][0]["b64"].as_str(),
            some(eq("<image:1048576 bytes>"))
        );
    }

    #[gtest]
    fn peels_dashscope_base64_aliases() {
        for key in ["base64", "image_base64"] {
            let raw = serde_json::json!({
                "output": {
                    "results": [{ key: large_image_data() }]
                }
            })
            .to_string();

            let Some(truncated) = truncate_raw(Some(raw)) else {
                panic!("raw string should remain Some after peeling");
            };

            let value: Value = serde_json::from_str(&truncated)
                .unwrap_or_else(|e| panic!("peeled DashScope JSON should parse: {e}"));
            expect_that!(
                value["output"]["results"][0][key].as_str(),
                some(eq("<image:1048576 bytes>"))
            );
        }
    }

    #[gtest]
    fn peels_imagen_base64_aliases() {
        for key in ["bytesBase64Encoded", "bytes_base64_encoded"] {
            let raw = serde_json::json!({
                "predictions": [{ key: large_image_data(), "mimeType": "image/png" }]
            })
            .to_string();

            let Some(truncated) = truncate_raw(Some(raw)) else {
                panic!("raw string should remain Some after peeling");
            };

            let value: Value = serde_json::from_str(&truncated)
                .unwrap_or_else(|e| panic!("peeled Imagen JSON should parse: {e}"));
            expect_that!(
                value["predictions"][0][key].as_str(),
                some(eq("<image:1048576 bytes>"))
            );
            expect_that!(
                value["predictions"][0]["mimeType"].as_str(),
                some(eq("image/png"))
            );
        }
    }

    #[gtest]
    fn peels_gemini_inline_data_field() {
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "inlineData": {
                            "mimeType": "image/png",
                            "data": large_image_data()
                        }
                    }]
                }
            }]
        })
        .to_string();

        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after peeling");
        };

        let value: Value = serde_json::from_str(&truncated)
            .unwrap_or_else(|e| panic!("peeled Gemini JSON should parse: {e}"));
        let inline_data = &value["candidates"][0]["content"]["parts"][0]["inlineData"];
        expect_that!(
            inline_data["data"].as_str(),
            some(eq("<image:1048576 bytes>"))
        );
        expect_that!(inline_data["mimeType"].as_str(), some(eq("image/png")));
    }

    #[gtest]
    fn peels_gemini_inline_data_snake_case_field() {
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "inline_data": {
                            "mime_type": "image/png",
                            "data": large_image_data()
                        }
                    }]
                }
            }]
        })
        .to_string();

        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after peeling");
        };

        let value: Value = serde_json::from_str(&truncated)
            .unwrap_or_else(|e| panic!("peeled Gemini JSON should parse: {e}"));
        let inline_data = &value["candidates"][0]["content"]["parts"][0]["inline_data"];
        expect_that!(
            inline_data["data"].as_str(),
            some(eq("<image:1048576 bytes>"))
        );
    }

    #[gtest]
    fn small_image_field_is_not_peeled() {
        let raw = serde_json::json!({
            "data": [{ "b64_json": "small-image-placeholder" }]
        })
        .to_string();

        assert_eq!(truncate_raw(Some(raw.clone())), Some(raw));
    }

    #[gtest]
    fn non_json_over_threshold_is_flat_truncated() {
        let raw = "not-json ".repeat(128 * 1024);
        let Some(truncated) = truncate_raw(Some(raw)) else {
            panic!("raw string should remain Some after truncation");
        };

        expect_that!(truncated.len(), le(RAW_RESPONSE_TRUNCATION_BYTES));
        expect_that!(truncated, contains_substring("<truncated:"));
    }
}
