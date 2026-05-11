use serde_json::{Map, Value};
use uuid::Uuid;

use crate::inference::types::FinishReason;
use crate::tool::ToolCallChunk;

const BUFFER_CAP: usize = 64 * 1024;
const OPEN_TOOL_CALLS_SINGLE: &str = "<｜DSML｜tool_calls>";
const CLOSE_TOOL_CALLS_SINGLE: &str = "</｜DSML｜tool_calls>";
const OPEN_INVOKE_SINGLE_PREFIX: &str = "<｜DSML｜invoke";
const CLOSE_INVOKE_SINGLE: &str = "</｜DSML｜invoke>";
const OPEN_PARAMETER_SINGLE_PREFIX: &str = "<｜DSML｜parameter";
const CLOSE_PARAMETER_SINGLE: &str = "</｜DSML｜parameter>";
const OPEN_TOOL_CALLS_DOUBLE: &str = "<｜｜DSML｜｜tool_calls>";
const CLOSE_TOOL_CALLS_DOUBLE: &str = "</｜｜DSML｜｜tool_calls>";
const OPEN_INVOKE_DOUBLE_PREFIX: &str = "<｜｜DSML｜｜invoke";
const CLOSE_INVOKE_DOUBLE: &str = "</｜｜DSML｜｜invoke>";
const OPEN_PARAMETER_DOUBLE_PREFIX: &str = "<｜｜DSML｜｜parameter";
const CLOSE_PARAMETER_DOUBLE: &str = "</｜｜DSML｜｜parameter>";
const END_OF_SENTENCE: &str = "<｜end▁of▁sentence｜>";

#[derive(Clone, Debug, Default)]
pub struct DsmlStreamState {
    pending: String,
    pipe_count: Option<u8>,
    emitted_call_seq: u32,
    pub had_successful_emission: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DsmlConsumed {
    pub text_passthrough: String,
    pub tool_calls: Vec<ToolCallChunk>,
    pub finish_override: Option<FinishReason>,
    pub diagnostics: Vec<DsmlDiagnostic>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DsmlDiagnostic {
    pub kind: DsmlDiagnosticKind,
    pub body_length: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DsmlDiagnosticKind {
    MissingCloseTag,
    JsonParseError,
    BufferCapExceeded,
    MissingInvokeName,
    MissingParameterName,
    MissingParameterStringFlag,
    PipeCountMismatch,
    MalformedTag,
}

#[derive(Clone, Copy)]
struct DsmlTokens {
    pipe_count: u8,
    open_tool_calls: &'static str,
    close_tool_calls: &'static str,
    open_invoke_prefix: &'static str,
    close_invoke: &'static str,
    open_parameter_prefix: &'static str,
    close_parameter: &'static str,
}

const SINGLE_TOKENS: DsmlTokens = DsmlTokens {
    pipe_count: 1,
    open_tool_calls: OPEN_TOOL_CALLS_SINGLE,
    close_tool_calls: CLOSE_TOOL_CALLS_SINGLE,
    open_invoke_prefix: OPEN_INVOKE_SINGLE_PREFIX,
    close_invoke: CLOSE_INVOKE_SINGLE,
    open_parameter_prefix: OPEN_PARAMETER_SINGLE_PREFIX,
    close_parameter: CLOSE_PARAMETER_SINGLE,
};

const DOUBLE_TOKENS: DsmlTokens = DsmlTokens {
    pipe_count: 2,
    open_tool_calls: OPEN_TOOL_CALLS_DOUBLE,
    close_tool_calls: CLOSE_TOOL_CALLS_DOUBLE,
    open_invoke_prefix: OPEN_INVOKE_DOUBLE_PREFIX,
    close_invoke: CLOSE_INVOKE_DOUBLE,
    open_parameter_prefix: OPEN_PARAMETER_DOUBLE_PREFIX,
    close_parameter: CLOSE_PARAMETER_DOUBLE,
};

impl DsmlStreamState {
    pub fn consume(
        &mut self,
        text: &str,
        is_final: bool,
        model_inference_id: Uuid,
    ) -> DsmlConsumed {
        if self.pending.is_empty() && !text.contains("DSML") && !text.contains('<') && !is_final {
            return DsmlConsumed {
                text_passthrough: text.to_string(),
                ..Default::default()
            };
        }

        self.pending.push_str(text);
        let mut consumed = DsmlConsumed::default();

        loop {
            if self.pending.is_empty() {
                break;
            }

            let Some((open_idx, tokens)) = self.find_next_accepted_open() else {
                if self.record_pipe_count_mismatch(&mut consumed) {
                    continue;
                }
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

            if self.pipe_count.is_none() {
                self.pipe_count = Some(tokens.pipe_count);
            }

            if self.pending.len() > BUFFER_CAP {
                consumed.diagnostics.push(DsmlDiagnostic {
                    kind: DsmlDiagnosticKind::BufferCapExceeded,
                    body_length: self.pending.len(),
                });
                self.pending.clear();
                break;
            }

            let body_start = tokens.open_tool_calls.len();
            let Some(close_idx) = self.pending[body_start..]
                .find(tokens.close_tool_calls)
                .map(|idx| idx + body_start)
            else {
                if is_final {
                    consumed.diagnostics.push(DsmlDiagnostic {
                        kind: DsmlDiagnosticKind::MissingCloseTag,
                        body_length: self.pending.len().saturating_sub(body_start),
                    });
                    self.pending.clear();
                }
                break;
            };

            let block_end = close_idx + tokens.close_tool_calls.len();
            let block = self.pending[..block_end].to_string();
            match parse_tool_calls_block(&block, tokens, model_inference_id, self.emitted_call_seq)
            {
                Ok(mut tool_calls) => {
                    self.emitted_call_seq += tool_calls.len() as u32;
                    if !tool_calls.is_empty() {
                        self.had_successful_emission = true;
                        consumed.finish_override = Some(FinishReason::ToolCall);
                    }
                    consumed.tool_calls.append(&mut tool_calls);
                }
                Err(diagnostic) => consumed.diagnostics.push(diagnostic),
            }
            self.pending.drain(..block_end);
        }

        consumed
    }

    fn find_next_accepted_open(&self) -> Option<(usize, DsmlTokens)> {
        let candidates = [SINGLE_TOKENS, DOUBLE_TOKENS];
        let mut best: Option<(usize, DsmlTokens)> = None;
        for tokens in candidates {
            if self
                .pipe_count
                .is_some_and(|count| count != tokens.pipe_count)
            {
                continue;
            }
            if let Some(idx) = self.pending.find(tokens.open_tool_calls)
                && best.is_none_or(|(best_idx, _)| idx < best_idx)
            {
                best = Some((idx, tokens));
            }
        }
        best
    }

    fn record_pipe_count_mismatch(&mut self, consumed: &mut DsmlConsumed) -> bool {
        let Some(locked) = self.pipe_count else {
            return false;
        };
        let mismatched = if locked == 1 {
            self.pending
                .find(OPEN_TOOL_CALLS_DOUBLE)
                .map(|idx| (idx, OPEN_TOOL_CALLS_DOUBLE.len()))
        } else {
            self.pending
                .find(OPEN_TOOL_CALLS_SINGLE)
                .map(|idx| (idx, OPEN_TOOL_CALLS_SINGLE.len()))
        };
        if let Some((idx, len)) = mismatched {
            let emit_len = idx + len;
            consumed
                .text_passthrough
                .push_str(&self.pending[..emit_len]);
            consumed.diagnostics.push(DsmlDiagnostic {
                kind: DsmlDiagnosticKind::PipeCountMismatch,
                body_length: len,
            });
            self.pending.drain(..emit_len);
            return true;
        }
        false
    }
}

fn parse_tool_calls_block(
    block: &str,
    tokens: DsmlTokens,
    model_inference_id: Uuid,
    call_seq_start: u32,
) -> Result<Vec<ToolCallChunk>, DsmlDiagnostic> {
    let body_start = tokens.open_tool_calls.len();
    let body_end = block
        .rfind(tokens.close_tool_calls)
        .ok_or_else(|| diagnostic(DsmlDiagnosticKind::MissingCloseTag, block.len()))?;
    let mut body = &block[body_start..body_end];
    body = body.trim();
    if body.ends_with(END_OF_SENTENCE) {
        body = body[..body.len() - END_OF_SENTENCE.len()].trim_end();
    }

    let mut cursor = 0;
    let mut tool_calls = Vec::new();
    while cursor < body.len() {
        cursor = skip_ws(body, cursor);
        if cursor >= body.len() {
            break;
        }
        if !body[cursor..].starts_with(tokens.open_invoke_prefix) {
            return Err(diagnostic(
                DsmlDiagnosticKind::MalformedTag,
                body.len().saturating_sub(cursor),
            ));
        }

        let invoke_tag_end = body[cursor..]
            .find('>')
            .map(|idx| idx + cursor)
            .ok_or_else(|| diagnostic(DsmlDiagnosticKind::MalformedTag, body.len() - cursor))?;
        let invoke_tag = &body[cursor..=invoke_tag_end];
        let invoke_name = attr(invoke_tag, "name")
            .ok_or_else(|| diagnostic(DsmlDiagnosticKind::MissingInvokeName, invoke_tag.len()))?;
        let invoke_body_start = invoke_tag_end + 1;
        let close_invoke_idx = body[invoke_body_start..]
            .find(tokens.close_invoke)
            .map(|idx| idx + invoke_body_start)
            .ok_or_else(|| {
                diagnostic(
                    DsmlDiagnosticKind::MissingCloseTag,
                    body.len().saturating_sub(invoke_body_start),
                )
            })?;
        let invoke_body = &body[invoke_body_start..close_invoke_idx];
        let args = parse_parameters(invoke_body, tokens)?;
        let raw_arguments = serde_json::to_string(&Value::Object(args))
            .map_err(|_| diagnostic(DsmlDiagnosticKind::JsonParseError, invoke_body.len()))?;
        let call_seq = call_seq_start + tool_calls.len() as u32;
        tool_calls.push(ToolCallChunk {
            id: format!("dsml-{model_inference_id}-{call_seq}"),
            raw_name: Some(invoke_name),
            raw_arguments,
        });
        cursor = close_invoke_idx + tokens.close_invoke.len();
    }

    Ok(tool_calls)
}

fn parse_parameters(body: &str, tokens: DsmlTokens) -> Result<Map<String, Value>, DsmlDiagnostic> {
    let mut cursor = 0;
    let mut args = Map::new();
    while cursor < body.len() {
        cursor = skip_ws(body, cursor);
        if cursor >= body.len() {
            break;
        }
        if !body[cursor..].starts_with(tokens.open_parameter_prefix) {
            return Err(diagnostic(
                DsmlDiagnosticKind::MalformedTag,
                body.len().saturating_sub(cursor),
            ));
        }
        let tag_end = body[cursor..]
            .find('>')
            .map(|idx| idx + cursor)
            .ok_or_else(|| diagnostic(DsmlDiagnosticKind::MalformedTag, body.len() - cursor))?;
        let tag = &body[cursor..=tag_end];
        let name = attr(tag, "name")
            .ok_or_else(|| diagnostic(DsmlDiagnosticKind::MissingParameterName, tag.len()))?;
        let string_flag = attr(tag, "string")
            .ok_or_else(|| diagnostic(DsmlDiagnosticKind::MissingParameterStringFlag, tag.len()))?;
        let value_start = tag_end + 1;
        let close_idx = body[value_start..]
            .find(tokens.close_parameter)
            .map(|idx| idx + value_start)
            .ok_or_else(|| {
                diagnostic(
                    DsmlDiagnosticKind::MissingCloseTag,
                    body.len().saturating_sub(value_start),
                )
            })?;
        let raw_value = &body[value_start..close_idx];
        let value = match string_flag.as_str() {
            "true" => Value::String(raw_value.to_string()),
            "false" => serde_json::from_str(raw_value)
                .map_err(|_| diagnostic(DsmlDiagnosticKind::JsonParseError, raw_value.len()))?,
            _ => {
                return Err(diagnostic(
                    DsmlDiagnosticKind::MissingParameterStringFlag,
                    tag.len(),
                ));
            }
        };
        args.insert(name, value);
        cursor = close_idx + tokens.close_parameter.len();
    }
    Ok(args)
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

fn diagnostic(kind: DsmlDiagnosticKind, body_length: usize) -> DsmlDiagnostic {
    DsmlDiagnostic { kind, body_length }
}

fn suspected_open_prefix_len(buffer: &str) -> usize {
    let open_tokens = [OPEN_TOOL_CALLS_SINGLE, OPEN_TOOL_CALLS_DOUBLE];
    let max_len = open_tokens
        .iter()
        .map(|token| token.len())
        .max()
        .unwrap_or(0)
        .min(buffer.len());
    for len in (1..=max_len).rev() {
        let tail = &buffer[buffer.len() - len..];
        if open_tokens.iter().any(|token| token.starts_with(tail)) {
            return len;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_uuid() -> Uuid {
        Uuid::parse_str("018cb37e-a000-7af7-84eb-04218e011e69").unwrap()
    }

    fn single_block(params: &str) -> String {
        format!(
            "{OPEN_TOOL_CALLS_SINGLE}<｜DSML｜invoke name=\"glob\">{params}</｜DSML｜invoke>{CLOSE_TOOL_CALLS_SINGLE}"
        )
    }

    fn double_block(params: &str) -> String {
        format!(
            "{OPEN_TOOL_CALLS_DOUBLE}<｜｜DSML｜｜invoke name=\"glob\">{params}</｜｜DSML｜｜invoke>{CLOSE_TOOL_CALLS_DOUBLE}"
        )
    }

    fn consume_all(input: &str) -> DsmlConsumed {
        let mut state = DsmlStreamState::default();
        state.consume(input, true, test_uuid())
    }

    #[test]
    fn parses_single_pipe_tool_call() {
        let input = single_block(
            "<｜DSML｜parameter name=\"pattern\" string=\"true\">/x/**</｜DSML｜parameter>",
        );
        let consumed = consume_all(&input);
        assert_eq!(consumed.text_passthrough, "");
        assert_eq!(consumed.finish_override, Some(FinishReason::ToolCall));
        assert_eq!(consumed.tool_calls.len(), 1);
        assert_eq!(consumed.tool_calls[0].raw_name.as_deref(), Some("glob"));
        assert_eq!(
            consumed.tool_calls[0].raw_arguments,
            r#"{"pattern":"/x/**"}"#
        );
    }

    #[test]
    fn parses_double_pipe_tool_call() {
        let input = double_block(
            "<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">/workspace/chunjiang/**</｜｜DSML｜｜parameter>",
        );
        let consumed = consume_all(&input);
        assert_eq!(consumed.diagnostics, vec![]);
        assert_eq!(
            consumed.tool_calls[0].raw_arguments,
            r#"{"pattern":"/workspace/chunjiang/**"}"#
        );
    }

    #[test]
    fn parses_multiple_invokes_with_distinct_ids() {
        let input = format!(
            "{OPEN_TOOL_CALLS_SINGLE}<｜DSML｜invoke name=\"glob\"><｜DSML｜parameter name=\"pattern\" string=\"true\">/a</｜DSML｜parameter></｜DSML｜invoke><｜DSML｜invoke name=\"read_file\"><｜DSML｜parameter name=\"path\" string=\"true\">/b</｜DSML｜parameter></｜DSML｜invoke>{CLOSE_TOOL_CALLS_SINGLE}"
        );
        let consumed = consume_all(&input);
        assert_eq!(consumed.tool_calls.len(), 2);
        assert_ne!(consumed.tool_calls[0].id, consumed.tool_calls[1].id);
        assert!(consumed.tool_calls[0].id.starts_with("dsml-"));
        assert_eq!(
            consumed.tool_calls[1].raw_name.as_deref(),
            Some("read_file")
        );
    }

    #[test]
    fn parses_typed_json_parameters() {
        let input = single_block(
            "<｜DSML｜parameter name=\"n\" string=\"false\">42</｜DSML｜parameter>\
             <｜DSML｜parameter name=\"obj\" string=\"false\">{\"a\":1}</｜DSML｜parameter>\
             <｜DSML｜parameter name=\"arr\" string=\"false\">[1,2,3]</｜DSML｜parameter>\
             <｜DSML｜parameter name=\"s\" string=\"false\">\"hello\"</｜DSML｜parameter>",
        );
        let consumed = consume_all(&input);
        let args: Value = serde_json::from_str(&consumed.tool_calls[0].raw_arguments).unwrap();
        assert_eq!(
            args,
            json!({"arr":[1,2,3],"n":42,"obj":{"a":1},"s":"hello"})
        );
    }

    #[test]
    fn keeps_literal_angle_in_string_parameter() {
        let input =
            single_block("<｜DSML｜parameter name=\"q\" string=\"true\">a < b</｜DSML｜parameter>");
        let consumed = consume_all(&input);
        assert_eq!(consumed.tool_calls[0].raw_arguments, r#"{"q":"a < b"}"#);
    }

    #[test]
    fn preserves_text_around_dsml_block() {
        let input = format!(
            "prefix {} suffix",
            single_block(
                "<｜DSML｜parameter name=\"pattern\" string=\"true\">/x</｜DSML｜parameter>"
            )
        );
        let consumed = consume_all(&input);
        assert_eq!(consumed.text_passthrough, "prefix  suffix");
        assert_eq!(consumed.tool_calls.len(), 1);
    }

    #[test]
    fn parses_per_byte_split_like_whole_blob() {
        let input = single_block(
            "<｜DSML｜parameter name=\"pattern\" string=\"true\">/x/**</｜DSML｜parameter>",
        );
        let mut state = DsmlStreamState::default();
        let mut out = DsmlConsumed::default();
        for ch in input.chars() {
            let consumed = state.consume(&ch.to_string(), false, test_uuid());
            out.text_passthrough.push_str(&consumed.text_passthrough);
            out.tool_calls.extend(consumed.tool_calls);
            out.diagnostics.extend(consumed.diagnostics);
            if consumed.finish_override.is_some() {
                out.finish_override = consumed.finish_override;
            }
        }
        let final_consumed = state.consume("", true, test_uuid());
        out.text_passthrough
            .push_str(&final_consumed.text_passthrough);
        out.tool_calls.extend(final_consumed.tool_calls);
        out.diagnostics.extend(final_consumed.diagnostics);
        if final_consumed.finish_override.is_some() {
            out.finish_override = final_consumed.finish_override;
        }
        let whole = consume_all(&input);
        assert_eq!(out.text_passthrough, whole.text_passthrough);
        assert_eq!(out.tool_calls, whole.tool_calls);
        assert_eq!(out.diagnostics, whole.diagnostics);
        assert_eq!(out.finish_override, whole.finish_override);
    }

    #[test]
    fn holds_split_open_marker_without_leaking_text() {
        let mut state = DsmlStreamState::default();
        let first = state.consume("<｜DSML", false, test_uuid());
        assert_eq!(first.text_passthrough, "");
        assert_eq!(state.pending, "<｜DSML");
        let second = state.consume(
            "｜tool_calls><｜DSML｜invoke name=\"glob\"><｜DSML｜parameter name=\"pattern\" string=\"true\">/x</｜DSML｜parameter></｜DSML｜invoke></｜DSML｜tool_calls>",
            false,
            test_uuid(),
        );
        assert_eq!(second.text_passthrough, "");
        assert_eq!(second.tool_calls.len(), 1);
    }

    #[test]
    fn drops_missing_close_at_final() {
        let mut state = DsmlStreamState::default();
        let input = "<｜DSML｜tool_calls><｜DSML｜invoke name=\"glob\"><｜DSML｜parameter name=\"pattern\" string=\"true\">/x";
        let consumed = state.consume(input, true, test_uuid());
        assert_eq!(consumed.text_passthrough, "");
        assert!(consumed.tool_calls.is_empty());
        assert_eq!(
            consumed.diagnostics[0].kind,
            DsmlDiagnosticKind::MissingCloseTag
        );
    }

    #[test]
    fn drops_invalid_json_parameter() {
        let input = single_block(
            "<｜DSML｜parameter name=\"n\" string=\"false\">not_json</｜DSML｜parameter>",
        );
        let consumed = consume_all(&input);
        assert!(consumed.tool_calls.is_empty());
        assert_eq!(
            consumed.diagnostics[0].kind,
            DsmlDiagnosticKind::JsonParseError
        );
    }

    #[test]
    fn drops_when_buffer_cap_exceeded() {
        let mut state = DsmlStreamState::default();
        let huge = format!(
            "{OPEN_TOOL_CALLS_SINGLE}<｜DSML｜invoke name=\"glob\">{}",
            "x".repeat(BUFFER_CAP + 1)
        );
        let consumed = state.consume(&huge, false, test_uuid());
        assert_eq!(
            consumed.diagnostics[0].kind,
            DsmlDiagnosticKind::BufferCapExceeded
        );
        assert_eq!(state.pending, "");
    }

    #[test]
    fn drops_invoke_without_name() {
        let input = format!(
            "{OPEN_TOOL_CALLS_SINGLE}<｜DSML｜invoke><｜DSML｜parameter name=\"pattern\" string=\"true\">/x</｜DSML｜parameter></｜DSML｜invoke>{CLOSE_TOOL_CALLS_SINGLE}"
        );
        let consumed = consume_all(&input);
        assert!(consumed.tool_calls.is_empty());
        assert_eq!(
            consumed.diagnostics[0].kind,
            DsmlDiagnosticKind::MissingInvokeName
        );
    }

    #[test]
    fn reports_pipe_count_mismatch_after_lock() {
        let mut state = DsmlStreamState::default();
        let first = state.consume(
            &double_block("<｜｜DSML｜｜parameter name=\"pattern\" string=\"true\">/x</｜｜DSML｜｜parameter>"),
            false,
            test_uuid(),
        );
        assert_eq!(first.tool_calls.len(), 1);
        let second = state.consume(OPEN_TOOL_CALLS_SINGLE, true, test_uuid());
        assert_eq!(
            second.diagnostics[0].kind,
            DsmlDiagnosticKind::PipeCountMismatch
        );
        assert_eq!(second.text_passthrough, OPEN_TOOL_CALLS_SINGLE);
    }
}
