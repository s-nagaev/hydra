//! Render-time extraction of the meaningful text of a stored request or
//! response body, for the "Content" blocks of the admin log detail page.
//!
//! Storage stays untouched: the bodies are the raw captures the logging
//! writer stored (a JSON document, or an SSE stream kept verbatim) and are
//! read here at render time only. The shapes handled are the ones Hydra
//! proxies, buffered and streamed: OpenAI Chat Completions
//! (`messages[].content`, `choices[].message.content`,
//! `choices[].delta.content`), OpenAI Responses (`instructions`, `input`,
//! `output[].content[].text`, `response.output_text.delta`) and Anthropic
//! Messages (`system`, `messages[].content[].text`, the `text_delta` of a
//! `content_block_delta`). Any other shape yields `None`, and the page then
//! renders no "Content" block for that direction: the full body below it
//! still carries everything. A JSON escape such as `\u041f` is decoded by the
//! JSON parse, the same decoding the full-body view applies to a body it can
//! parse.

use serde_json::Value;

/// The text of a stored request body, one `role: text` line per message. An
/// Anthropic `system` prompt, a Responses `instructions` string and the items
/// of a Responses `input` list contribute as well. `None` when the body is
/// not JSON, or is JSON with nothing recognizable in it.
pub(crate) fn request_content(raw: &str) -> Option<String> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let mut lines: Vec<String> = Vec::new();
    for key in ["instructions", "system"] {
        if let Some(text) = content_text(value.get(key)) {
            push_line(&mut lines, key, &text);
        }
    }
    for key in ["messages", "input"] {
        push_items(&mut lines, value.get(key));
    }
    joined(lines)
}

/// The text of a stored response body: the answer of a buffered response, or
/// the text deltas of a stored SSE stream concatenated in stream order.
/// `None` when there is nothing to read.
pub(crate) fn response_content(raw: &str) -> Option<String> {
    match serde_json::from_str(raw) {
        Ok(value) => buffered_response_content(&value),
        Err(_) => sse_response_content(raw),
    }
}

/// Text of a buffered response in each surface's own shape:
/// `choices[].message.content` for Chat, `output[].content[].text` (or the
/// convenience `output_text` a Responses object may carry when it lists no
/// output items) for Responses, and the top-level `content` blocks for
/// Anthropic. A response that reports only tool calls or reasoning
/// contributes nothing.
fn buffered_response_content(value: &Value) -> Option<String> {
    let mut pieces: Vec<String> = Vec::new();
    for item in array_of(value.get("output")) {
        if let Some(text) = content_text(item.get("content")) {
            pieces.push(text);
        }
    }
    for choice in array_of(value.get("choices")) {
        let message = choice.get("message");
        if let Some(text) = content_text(message.and_then(|message| message.get("content"))) {
            pieces.push(text);
        }
    }
    if let Some(text) = content_text(value.get("content")) {
        pieces.push(text);
    }
    if pieces.is_empty() {
        if let Some(text) = value.get("output_text").and_then(Value::as_str) {
            pieces.push(text.to_string());
        }
    }
    non_empty(pieces.join("\n\n"))
}

/// Text of a stored SSE capture: every payload that carries a text delta is
/// appended in stream order, so the concatenation reads as what the client
/// assembled from the stream. Framing lines, the `[DONE]` sentinel and
/// payloads without text (usage chunks, tool-call deltas, lifecycle events)
/// contribute nothing. A capture cut at the 1 MiB body cap still yields the
/// text of the frames that did land.
fn sse_response_content(raw: &str) -> Option<String> {
    let mut text = String::new();
    for line in raw.lines() {
        let Some(payload) = line.trim_end_matches('\r').strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim_start();
        if payload == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        append_stream_deltas(&value, &mut text);
    }
    non_empty(text)
}

/// Appends the text deltas of one streamed payload. Chat carries them at
/// `choices[].delta.content`, one per choice for a stream that fans out over
/// several; Anthropic at the `text` of a `content_block_delta` delta object;
/// Responses at the `delta` string of `response.output_text.delta`.
fn append_stream_deltas(value: &Value, text: &mut String) {
    if let Some(delta) = value.get("delta").and_then(delta_text) {
        text.push_str(&delta);
    }
    for choice in array_of(value.get("choices")) {
        if let Some(delta) = choice.get("delta").and_then(delta_text) {
            text.push_str(&delta);
        }
    }
}

/// The text of one stream delta: a bare string (Responses `output_text`
/// deltas), or an object that carries it under `content` (Chat) or under
/// `text` (Anthropic `text_delta`). Any other delta, `input_json_delta`
/// included, has no text to read.
fn delta_text(delta: &Value) -> Option<String> {
    match delta {
        Value::String(text) => non_empty(text.clone()),
        Value::Object(object) => match object.get("content").and_then(Value::as_str) {
            Some(content) => non_empty(content.to_string()),
            None => object
                .get("text")
                .and_then(Value::as_str)
                .and_then(|text| non_empty(text.to_string())),
        },
        _ => None,
    }
}

/// Appends one `label: text` line per entry of a `messages` list or of a
/// Responses `input` field, both of which are a bare string or a list of
/// items.
fn push_items(lines: &mut Vec<String>, value: Option<&Value>) {
    match value {
        Some(Value::String(text)) => push_line(lines, "input", text),
        Some(Value::Array(items)) => items.iter().for_each(|item| push_item(lines, item)),
        _ => {}
    }
}

/// One entry of `messages` or of a Responses `input` list. Its text is the
/// `content` field, a string or a list of parts. An item without one still
/// contributes a bare `text` field (a Responses input part), and a
/// function-call result contributes its `output` string.
fn push_item(lines: &mut Vec<String>, item: &Value) {
    if let Some(text) = item.as_str() {
        push_line(lines, "input", text);
        return;
    }
    let Some(object) = item.as_object() else {
        return;
    };
    let label = match object.get("role").and_then(Value::as_str) {
        Some(role) => role,
        None if object.contains_key("output") => "tool output",
        None => "input",
    };
    if let Some(text) = content_text(object.get("content")) {
        push_line(lines, label, &text);
        return;
    }
    for key in ["text", "output"] {
        if let Some(text) = object.get(key).and_then(Value::as_str) {
            push_line(lines, label, text);
            return;
        }
    }
}

/// The text of a content-bearing value: a bare string, or a list of parts of
/// which each contributes its own text. Parts that carry no text (images,
/// audio, a tool-call request) contribute nothing; a part boundary reads as
/// a line break, and the whitespace a part ends with is dropped there
/// (invisible in the rendered block anyway).
fn content_text(value: Option<&Value>) -> Option<String> {
    let pieces: Vec<String> = match value? {
        Value::String(text) => vec![text.clone()],
        Value::Array(parts) => parts
            .iter()
            .filter_map(part_text)
            .map(|piece| piece.trim_end().to_string())
            .collect(),
        _ => return None,
    };
    non_empty(pieces.join("\n"))
}

/// The text of one content part: the `text` field of a text part, or the
/// nested `content` of a tool result, whose own shape repeats the string or
/// list of parts.
fn part_text(part: &Value) -> Option<String> {
    if let Some(text) = part.as_str() {
        return Some(text.to_string());
    }
    let object = part.as_object()?;
    match object.get("text").and_then(Value::as_str) {
        Some(text) => non_empty(text.to_string()),
        None => content_text(object.get("content")),
    }
}

/// Appends one `label: text` line; whitespace-only text adds no line, so an
/// empty message never becomes an empty entry.
fn push_line(lines: &mut Vec<String>, label: &str, text: &str) {
    let text = text.trim();
    if !text.is_empty() {
        lines.push(format!("{label}: {text}"));
    }
}

/// The joined lines, or `None` when there is no line to show.
fn joined(lines: Vec<String>) -> Option<String> {
    non_empty(lines.join("\n"))
}

/// `Some` for text with something but whitespace in it; `None` otherwise, so
/// an empty direction renders no block at all instead of an empty one. The
/// text itself is never trimmed here: a stream delta keeps the spaces it
/// carries, which is what makes the concatenation read as the answer.
fn non_empty(text: String) -> Option<String> {
    (!text.trim().is_empty()).then_some(text)
}

/// The array behind a value, or no items at all when it is absent or of
/// another type.
fn array_of(value: Option<&Value>) -> impl Iterator<Item = &Value> {
    value.and_then(Value::as_array).into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_request_lists_every_message_with_its_role() {
        let raw = r#"{"model":"gpt-4o","messages":[{"role":"system","content":"Be brief."},{"role":"user","content":"Hello"}]}"#;
        assert_eq!(
            request_content(raw).as_deref(),
            Some("system: Be brief.\nuser: Hello")
        );
    }

    #[test]
    fn a_chat_request_reads_the_text_parts_of_a_content_list() {
        let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"What is this?"},{"type":"image_url","image_url":{"url":"https://example.test/a.png"}}]}]}"#;
        assert_eq!(request_content(raw).as_deref(), Some("user: What is this?"));
    }

    #[test]
    fn an_anthropic_request_reads_the_system_prompt_and_text_blocks() {
        let raw = r#"{"model":"claude","system":[{"type":"text","text":"Be brief."}],"messages":[{"role":"user","content":[{"type":"text","text":"Hello"},{"type":"image","source":{"type":"base64","data":"AAAA"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"42"}]}]}"#;
        assert_eq!(
            request_content(raw).as_deref(),
            Some("system: Be brief.\nuser: Hello\nuser: 42")
        );
    }

    #[test]
    fn a_responses_request_reads_its_instructions_and_its_input() {
        let raw = r#"{"model":"gpt-5","instructions":"Be terse.","input":"What is 2+2?"}"#;
        assert_eq!(
            request_content(raw).as_deref(),
            Some("instructions: Be terse.\ninput: What is 2+2?")
        );
    }

    #[test]
    fn a_responses_input_list_keeps_the_role_of_each_item() {
        let raw = r#"{"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Hi"}]},{"type":"input_text","text":"More"},{"type":"function_call_output","call_id":"call_1","output":"42"}]}"#;
        assert_eq!(
            request_content(raw).as_deref(),
            Some("user: Hi\ninput: More\ntool output: 42")
        );
    }

    #[test]
    fn a_message_without_content_is_skipped_not_left_as_an_empty_line() {
        // An assistant turn that only asks for a tool call carries no text.
        let raw = r#"{"messages":[{"role":"user","content":"weather?"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_1"}]}]}"#;
        assert_eq!(request_content(raw).as_deref(), Some("user: weather?"));
    }

    #[test]
    fn unicode_escapes_are_decoded_in_the_extracted_request_content() {
        let raw =
            r#"{"messages":[{"role":"user","content":"\u041f\u0440\u0438\u0432\u0435\u0442"}]}"#;
        assert_eq!(request_content(raw).as_deref(), Some("user: Привет"));
    }

    #[test]
    fn a_request_that_is_not_json_or_carries_no_text_has_no_content() {
        assert_eq!(request_content("upstream returned 502"), None);
        assert_eq!(request_content(r#"{"prompt":"\u041f"#), None);
        assert_eq!(request_content(r#"{"model":"gpt-4o","stream":true}"#), None);
        assert_eq!(request_content(r#"{"messages":[]}"#), None);
        assert_eq!(
            request_content(r#"{"messages":[{"role":"user","content":[{"type":"image_url"}]}]}"#),
            None
        );
    }

    #[test]
    fn a_chat_response_reads_the_message_content() {
        let raw = r#"{"id":"cmpl-1","choices":[{"index":0,"message":{"role":"assistant","content":"Hello there"}}]}"#;
        assert_eq!(response_content(raw).as_deref(), Some("Hello there"));
    }

    #[test]
    fn a_chat_response_with_several_choices_keeps_both_answers() {
        let raw =
            r#"{"choices":[{"message":{"content":"First"}},{"message":{"content":"Second"}}]}"#;
        assert_eq!(response_content(raw).as_deref(), Some("First\n\nSecond"));
    }

    #[test]
    fn a_responses_response_reads_the_output_text_parts() {
        let raw = r#"{"id":"resp_1","output":[{"type":"reasoning","summary":[]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Hello "},{"type":"output_text","text":"there"}]}]}"#;
        assert_eq!(response_content(raw).as_deref(), Some("Hello\nthere"));
    }

    #[test]
    fn a_responses_response_falls_back_to_the_convenience_output_text() {
        // Some Responses objects carry the assembled answer in `output_text`
        // and list no output items; the duplicates of a body that has both
        // are not repeated.
        let without_items = r#"{"output_text":"Hello there","output":[]}"#;
        assert_eq!(
            response_content(without_items).as_deref(),
            Some("Hello there")
        );
        let with_items = r#"{"output_text":"Hello there","output":[{"type":"message","content":[{"type":"output_text","text":"Hello there"}]}]}"#;
        assert_eq!(response_content(with_items).as_deref(), Some("Hello there"));
    }

    #[test]
    fn an_anthropic_response_reads_its_text_blocks_only() {
        let raw = r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"Let me check."},{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{"city":"Krakow"}},{"type":"text","text":"One moment."}]}"#;
        assert_eq!(
            response_content(raw).as_deref(),
            Some("Let me check.\nOne moment.")
        );
    }

    #[test]
    fn a_response_without_any_text_has_no_content() {
        let calls_only =
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"call_1"}]}}]}"#;
        assert_eq!(response_content(calls_only), None);
        assert_eq!(
            response_content(r#"{"error":{"message":"bad model"}}"#),
            None
        );
        assert_eq!(response_content(r#"{"usage":{"prompt_tokens":5}}"#), None);
    }

    #[test]
    fn a_response_that_is_not_json_has_no_content() {
        assert_eq!(response_content("upstream error: Ошибка"), None);
        assert_eq!(response_content(""), None);
    }

    #[test]
    fn a_chat_stream_concatenates_the_content_deltas() {
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(response_content(raw).as_deref(), Some("Hello"));
    }

    #[test]
    fn a_chat_stream_keeps_the_text_of_a_tool_call_delta_out() {
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Wait\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n",
        );
        assert_eq!(response_content(raw).as_deref(), Some("Wait"));
    }

    #[test]
    fn a_responses_stream_concatenates_the_output_text_deltas() {
        let raw = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
        );
        assert_eq!(response_content(raw).as_deref(), Some("Hello"));
    }

    #[test]
    fn an_anthropic_stream_concatenates_its_text_deltas_and_ignores_the_rest() {
        let raw = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        );
        assert_eq!(response_content(raw).as_deref(), Some("Hello"));
    }

    #[test]
    fn stream_framing_lines_and_unreadable_payloads_contribute_nothing() {
        let raw = concat!(
            "event: ping\n",
            ": keep-alive\n\n",
            "data: {not json}\n\n",
            "data: [DONE]\n\n",
        );
        assert_eq!(response_content(raw), None);
    }

    #[test]
    fn unicode_escapes_are_decoded_in_a_streamed_response() {
        let raw = "data: {\"choices\":[{\"delta\":{\"content\":\"\\u041f\\u0440\\u0438\\u0432\\u0435\\u0442\"}}]}\n\n";
        assert_eq!(response_content(raw).as_deref(), Some("Привет"));
    }

    #[test]
    fn a_stream_cut_at_the_capture_cap_still_yields_the_frames_that_landed() {
        // The capture stops mid-frame at the 1 MiB cap; the complete frames
        // before it are still worth reading.
        let raw = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"co",
        );
        assert_eq!(response_content(raw).as_deref(), Some("Hel"));
    }
}
