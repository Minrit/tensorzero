use serde_json::{Map, Value};
use uuid::Uuid;

use crate::tool::{ToolCall, ToolCallChunk};

const OPEN_FUNCTION_CALLS: &str = "<function_calls>";
const CLOSE_FUNCTION_CALLS: &str = "</function_calls>";
const OPEN_INVOKE_PREFIX: &str = "<invoke";
const CLOSE_INVOKE: &str = "</invoke>";
const OPEN_PARAMETER_PREFIX: &str = "<parameter";
const CLOSE_PARAMETER: &str = "</parameter>";
const BUFFER_CAP: usize = 64 * 1024;

#[derive(Debug, Default, PartialEq)]
pub struct ClaudeXmlConsumed {
    pub text_passthrough: String,
    pub tool_calls: Vec<ToolCallChunk>,
}

#[derive(Debug, Default)]
pub struct ClaudeXmlStreamState {
    pending: String,
    emitted_call_seq: u32,
    pub had_successful_emission: bool,
}

pub fn recover_claude_xml_tool_calls(text: &str, model_inference_id: Uuid) -> ClaudeXmlConsumed {
    let mut state = ClaudeXmlStreamState::default();
    state.consume(text, true, model_inference_id)
}

impl ClaudeXmlStreamState {
    pub fn consume(
        &mut self,
        text: &str,
        is_final: bool,
        model_inference_id: Uuid,
    ) -> ClaudeXmlConsumed {
        if self.pending.is_empty() && !text.contains('<') && !is_final {
            return ClaudeXmlConsumed {
                text_passthrough: text.to_string(),
                tool_calls: Vec::new(),
            };
        }

        self.pending.push_str(text);
        let mut consumed = ClaudeXmlConsumed::default();

        loop {
            if self.pending.is_empty() {
                break;
            }

            let Some(open_idx) = self.pending.find(OPEN_FUNCTION_CALLS) else {
                if is_final {
                    consumed.text_passthrough.push_str(&self.pending);
                    self.pending.clear();
                    break;
                }

                let hold = suspected_open_prefix_len(&self.pending);
                let emit_len = self.pending.len().saturating_sub(hold);
                if emit_len > 0 {
                    consumed
                        .text_passthrough
                        .push_str(&self.pending[..emit_len]);
                    self.pending.drain(..emit_len);
                }
                break;
            };

            if open_idx > 0 {
                consumed
                    .text_passthrough
                    .push_str(&self.pending[..open_idx]);
                self.pending.drain(..open_idx);
            }

            if self.pending.len() > BUFFER_CAP {
                self.pending.clear();
                break;
            }

            let body_start = OPEN_FUNCTION_CALLS.len();
            let Some(close_idx) = self.pending[body_start..]
                .find(CLOSE_FUNCTION_CALLS)
                .map(|idx| idx + body_start)
            else {
                if is_final {
                    self.pending.clear();
                }
                break;
            };

            let block_end = close_idx + CLOSE_FUNCTION_CALLS.len();
            let body = &self.pending[body_start..close_idx];
            match parse_function_calls_body(body, model_inference_id, self.emitted_call_seq) {
                Some(mut tool_calls) if !tool_calls.is_empty() => {
                    self.emitted_call_seq += tool_calls.len() as u32;
                    self.had_successful_emission = true;
                    consumed.tool_calls.append(&mut tool_calls);
                }
                _ => {}
            }
            self.pending.drain(..block_end);
        }

        consumed
    }
}

pub fn tool_call_chunk_to_tool_call(tool_call: ToolCallChunk) -> ToolCall {
    ToolCall {
        id: tool_call.id,
        name: tool_call.raw_name.unwrap_or_default(),
        arguments: tool_call.raw_arguments,
    }
}

fn parse_function_calls_body(
    body: &str,
    model_inference_id: Uuid,
    call_seq_start: u32,
) -> Option<Vec<ToolCallChunk>> {
    let mut cursor = 0usize;
    let mut tool_calls = Vec::new();

    while cursor < body.len() {
        cursor = skip_ws(body, cursor);
        if cursor >= body.len() {
            break;
        }
        if !body[cursor..].starts_with(OPEN_INVOKE_PREFIX) {
            return None;
        }

        let invoke_tag_end = body[cursor..].find('>').map(|idx| idx + cursor)?;
        let invoke_tag = &body[cursor..=invoke_tag_end];
        let invoke_name = attr(invoke_tag, "name")?;
        let invoke_body_start = invoke_tag_end + 1;
        let close_invoke_idx = body[invoke_body_start..]
            .find(CLOSE_INVOKE)
            .map(|idx| idx + invoke_body_start)?;
        let invoke_body = &body[invoke_body_start..close_invoke_idx];
        let args = parse_parameters(invoke_body)?;
        let raw_arguments = serde_json::to_string(&Value::Object(args)).ok()?;
        let call_seq = call_seq_start + tool_calls.len() as u32;
        tool_calls.push(ToolCallChunk {
            id: format!("claude-xml-{model_inference_id}-{call_seq}"),
            raw_name: Some(invoke_name),
            raw_arguments,
        });
        cursor = close_invoke_idx + CLOSE_INVOKE.len();
    }

    Some(tool_calls)
}

fn parse_parameters(body: &str) -> Option<Map<String, Value>> {
    let mut cursor = 0usize;
    let mut args = Map::new();

    while cursor < body.len() {
        cursor = skip_ws(body, cursor);
        if cursor >= body.len() {
            break;
        }
        if !body[cursor..].starts_with(OPEN_PARAMETER_PREFIX) {
            return None;
        }

        let tag_end = body[cursor..].find('>').map(|idx| idx + cursor)?;
        let tag = &body[cursor..=tag_end];
        let name = attr(tag, "name")?;
        let value_start = tag_end + 1;
        let close_idx = body[value_start..]
            .find(CLOSE_PARAMETER)
            .map(|idx| idx + value_start)?;
        let raw_value = &body[value_start..close_idx];
        let value = match attr(tag, "string").as_deref() {
            Some("false") => serde_json::from_str(raw_value).ok()?,
            _ => Value::String(raw_value.to_string()),
        };
        args.insert(name, value);
        cursor = close_idx + CLOSE_PARAMETER.len();
    }

    Some(args)
}

fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')? + start;
    Some(tag[start..end].to_string())
}

fn skip_ws(text: &str, mut cursor: usize) -> usize {
    while cursor < text.len() {
        let Some(ch) = text[cursor..].chars().next() else {
            break;
        };
        if !ch.is_whitespace() {
            break;
        }
        cursor += ch.len_utf8();
    }
    cursor
}

fn suspected_open_prefix_len(buffer: &str) -> usize {
    let max_len = OPEN_FUNCTION_CALLS.len().min(buffer.len());
    for len in (1..=max_len).rev() {
        let tail = &buffer[buffer.len() - len..];
        if OPEN_FUNCTION_CALLS.starts_with(tail) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recover_claude_xml_tool_calls_removes_xml_and_emits_tool_call() {
        let id = Uuid::nil();
        let text = concat!(
            "before\n",
            "<function_calls>\n",
            "<invoke name=\"write_file\">\n",
            "<parameter name=\"path\">/workspace/index.html</parameter>\n",
            "<parameter name=\"content\"><!DOCTYPE html></parameter>\n",
            "</invoke>\n",
            "</function_calls>\n",
            "after",
        );

        let consumed = recover_claude_xml_tool_calls(text, id);

        assert_eq!(consumed.text_passthrough, "before\n\nafter");
        assert_eq!(consumed.tool_calls.len(), 1);
        let call = &consumed.tool_calls[0];
        assert_eq!(call.id, "claude-xml-00000000-0000-0000-0000-000000000000-0");
        assert_eq!(call.raw_name.as_deref(), Some("write_file"));
        assert_eq!(
            serde_json::from_str::<Value>(&call.raw_arguments).unwrap(),
            serde_json::json!({
                "content": "<!DOCTYPE html>",
                "path": "/workspace/index.html",
            })
        );
    }

    #[test]
    fn recover_claude_xml_tool_calls_preserves_malformed_xml() {
        let text = "before <function_calls><invoke></invoke></function_calls> after";

        let consumed = recover_claude_xml_tool_calls(text, Uuid::nil());

        assert_eq!(consumed.text_passthrough, "before  after");
        assert!(consumed.tool_calls.is_empty());
    }

    #[test]
    fn stream_state_buffers_split_function_calls_until_close() {
        let mut state = ClaudeXmlStreamState::default();
        let id = Uuid::nil();

        let first = state.consume(
            "before <function_calls><invoke name=\"Task\"><parameter name=\"type\">explore",
            false,
            id,
        );
        let second = state.consume("</parameter></invoke></function_calls> after", false, id);

        assert_eq!(first.text_passthrough, "before ");
        assert!(first.tool_calls.is_empty());
        assert_eq!(second.text_passthrough, " after");
        assert_eq!(second.tool_calls[0].raw_name.as_deref(), Some("Task"));
    }
}
