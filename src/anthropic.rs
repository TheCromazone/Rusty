//! Anthropic's Messages API: request building from rusty's OpenAI-shaped
//! history, and the SSE stream back into a `Reply`.
//!
//! rusty keeps its history in the OpenAI chat format. An assistant turn that
//! came from Claude also carries the raw content blocks under `_anthropic`, so
//! thinking blocks go back to the API exactly as they arrived. The history is
//! not append-only (compaction, elision, a fresh system prompt each request),
//! so on the models that check it rusty asks the API to drop a thinking block
//! whose conversation changed rather than reject the request.

use anyhow::{bail, Context, Result};
use reqwest::blocking::Response;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::Read;

use crate::execution::ExecutionMode;
use crate::llm::{Delta, Reply, ToolCall, Usage};
use crate::sse::{Lines, Obj, Str, U64};

pub const VERSION: &str = "2023-06-01";
/// Lets a request ask for `prefix_mismatch_behavior`.
const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";
/// `fallbacks: "default"`: a declined request is re-run on Anthropic's
/// recommended fallback model inside the same call.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// Key under which an assistant message keeps Claude's raw content blocks.
pub const BLOCKS_KEY: &str = "_anthropic";
const MAX_TOKENS: u64 = 64000;

/// What a Claude model accepts, by id. Unknown ids get the conservative
/// answer: no thinking settings at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// `thinking: {type: "adaptive"}` and `output_config.effort`.
    pub adaptive: bool,
    /// The `xhigh` effort level.
    pub xhigh: bool,
    /// Checks thinking blocks against the conversation (`block_binding`).
    pub binding: bool,
    /// Accepts `fallbacks: "default"`.
    pub fallbacks: bool,
}

pub fn caps(model: &str) -> Caps {
    let m = model.to_ascii_lowercase();
    let fifth =
        ["claude-fable-5", "claude-mythos-5", "claude-opus-5", "claude-sonnet-5"].iter().any(|p| m.starts_with(p));
    let late_fourth =
        ["claude-opus-4-6", "claude-sonnet-4-6", "claude-opus-4-7", "claude-opus-4-8"].iter().any(|p| m.starts_with(p));
    Caps {
        adaptive: fifth || late_fourth,
        xhigh: fifth || m.starts_with("claude-opus-4-7") || m.starts_with("claude-opus-4-8"),
        binding: fifth,
        fallbacks: ["claude-fable-5-1", "claude-opus-5-5", "claude-opus-5", "claude-sonnet-5-5"]
            .iter()
            .any(|p| m == *p || m.starts_with(&format!("{p}-"))),
    }
}

/// The request body and the `anthropic-beta` header value it needs.
pub fn request(
    model: &str,
    messages: &[Value],
    tools: &Value,
    mode: ExecutionMode,
    first_party: bool,
) -> (Value, Option<String>) {
    let (system, msgs) = convert(messages);
    let caps = caps(model);
    // Claude 3 models stop at 8,192 output tokens and reject more.
    let max_tokens = if model.starts_with("claude-3") { 8192 } else { MAX_TOKENS };
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": msgs,
        // Caches the longest stable prefix (tools, then system, then history).
        "cache_control": {"type": "ephemeral"},
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    let defs = tool_defs(tools);
    if !defs.is_empty() {
        body["tools"] = Value::Array(defs);
        body["tool_choice"] = json!({"type": "auto"});
    }
    let mut betas = Vec::new();
    if caps.adaptive {
        let mut thinking = json!({"type": "adaptive", "display": "summarized"});
        if caps.binding && first_party {
            thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
            betas.push(BINDING_BETA);
        }
        body["thinking"] = thinking;
        let effort = match mode {
            ExecutionMode::Careful if caps.xhigh => "xhigh",
            ExecutionMode::Careful | ExecutionMode::Standard => "high",
            ExecutionMode::Vibe => "low",
        };
        body["output_config"] = json!({"effort": effort});
    }
    if caps.fallbacks && first_party {
        body["fallbacks"] = json!("default");
        betas.push(FALLBACK_BETA);
    }
    (body, (!betas.is_empty()).then(|| betas.join(",")))
}

/// OpenAI function tools → Anthropic tools. Inputs stream as they are
/// written (a long `write_file` shows progress instead of a silent wait);
/// the agent already parses every input strictly and answers bad JSON with
/// an error result instead of running the tool.
fn tool_defs(tools: &Value) -> Vec<Value> {
    tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = t.get("function")?;
            Some(json!({
                "name": f["name"],
                "description": f["description"].as_str().unwrap_or(""),
                "input_schema": if f["parameters"].is_object() { f["parameters"].clone() } else { json!({"type": "object"}) },
                "eager_input_streaming": true,
            }))
        })
        .collect()
}

/// rusty's OpenAI-shaped messages → (system text, Anthropic messages).
///
/// System messages join the top-level system prompt. Tool results become
/// `tool_result` blocks, merged into one user turn per batch. A `tool_use`
/// whose result is gone (an interrupted turn, an elided history) gets a stub
/// result so the request stays valid, and a result whose call is gone
/// becomes plain text.
pub fn convert(messages: &[Value]) -> (String, Vec<Value>) {
    let mut system: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m["role"].as_str().unwrap_or("") {
            "system" => {
                if let Some(s) = m["content"].as_str().filter(|s| !s.trim().is_empty()) {
                    system.push(s.to_string());
                }
            }
            "assistant" => {
                let blocks = assistant_blocks(m);
                if !blocks.is_empty() {
                    out.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            "tool" => {
                let text = text_of(&m["content"]);
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m["tool_call_id"].as_str().unwrap_or(""),
                    // The API rejects an empty result; a silent command still answered.
                    "content": if text.trim().is_empty() { "(no output)".to_string() } else { text },
                });
                push_user(&mut out, block);
            }
            _ => {
                let text = text_of(&m["content"]);
                if !text.trim().is_empty() {
                    push_user(&mut out, json!({"type": "text", "text": text}));
                }
            }
        }
    }
    pair_tool_results(&mut out);
    if out.first().is_some_and(|m| m["role"] != "user") {
        out.insert(0, json!({"role": "user", "content": [{"type": "text", "text": "(continuing the session)"}]}));
    }
    (system.join("\n\n"), out)
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n"),
        other => other.to_string(),
    }
}

/// Claude's own blocks when the turn came from Claude, else blocks rebuilt
/// from the OpenAI fields.
fn assistant_blocks(m: &Value) -> Vec<Value> {
    if let Some(raw) = m[BLOCKS_KEY].as_array().filter(|b| !b.is_empty()) {
        // A whitespace-only text block is rejected when sent back.
        return raw
            .iter()
            .filter(|b| b["type"] != "text" || b["text"].as_str().is_some_and(|t| !t.trim().is_empty()))
            .cloned()
            .collect();
    }
    let mut blocks = Vec::new();
    if let Some(text) = m["content"].as_str().filter(|s| !s.trim().is_empty()) {
        blocks.push(json!({"type": "text", "text": text}));
    }
    for c in m["tool_calls"].as_array().into_iter().flatten() {
        let args = c["function"]["arguments"].as_str().unwrap_or("");
        let input = serde_json::from_str::<Value>(args).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": c["id"].as_str().unwrap_or(""),
            "name": c["function"]["name"].as_str().unwrap_or(""),
            "input": input,
        }));
    }
    blocks
}

fn push_user(out: &mut Vec<Value>, block: Value) {
    if let Some(last) = out.last_mut().filter(|m| m["role"] == "user") {
        last["content"].as_array_mut().expect("user content is an array").push(block);
    } else {
        out.push(json!({"role": "user", "content": [block]}));
    }
}

/// Every `tool_use` gets a `tool_result` in the very next user turn, results
/// come first in that turn, and no result is left without its call.
fn pair_tool_results(out: &mut Vec<Value>) {
    let mut i = 0;
    while i < out.len() {
        let ids: Vec<String> = if out[i]["role"] == "assistant" {
            out[i]["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| b["id"].as_str().map(String::from))
                .collect()
        } else {
            Vec::new()
        };
        if out[i]["role"] == "user" {
            let prev: Vec<String> = i
                .checked_sub(1)
                .and_then(|p| out[p]["content"].as_array())
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| b["id"].as_str().map(String::from))
                .collect();
            let blocks = out[i]["content"].as_array_mut().expect("user content is an array");
            for b in blocks.iter_mut() {
                if b["type"] == "tool_result" && !prev.iter().any(|id| b["tool_use_id"] == id.as_str()) {
                    *b = json!({"type": "text", "text": format!("(tool output) {}", text_of(&b["content"]))});
                }
            }
            // Results first: the API reads the turn's leading blocks as the answers.
            blocks.sort_by_key(|b| b["type"] != "tool_result");
        }
        if !ids.is_empty() {
            if out.get(i + 1).is_none_or(|m| m["role"] != "user") {
                out.insert(i + 1, json!({"role": "user", "content": []}));
            }
            let next = out[i + 1]["content"].as_array_mut().expect("user content is an array");
            for id in ids.iter().rev() {
                if !next.iter().any(|b| b["type"] == "tool_result" && b["tool_use_id"] == id.as_str()) {
                    next.insert(
                        0,
                        json!({"type": "tool_result", "tool_use_id": id, "content": "(no result: the call did not run)"}),
                    );
                }
            }
        }
        i += 1;
    }
}

/// Reads the SSE stream into a `Reply`. Text and summarised thinking go to
/// `on` as they arrive; returning false stops the stream.
pub fn parse_stream(resp: Response, on: &mut dyn FnMut(Delta) -> bool) -> Result<Reply> {
    parse_lines(&mut crate::llm::lines(resp), on)
}

/// An event read in place (see `crate::sse`); only text, thinking and tool
/// input deltas, the bulk of a stream, are read this way.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Event<'a> {
    #[serde(rename = "type", borrow)]
    kind: Str<'a>,
    index: U64,
    #[serde(borrow)]
    delta: Obj<BlockDelta<'a>>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BlockDelta<'a> {
    #[serde(rename = "type", borrow)]
    kind: Str<'a>,
    #[serde(borrow)]
    text: Str<'a>,
    #[serde(borrow)]
    thinking: Str<'a>,
    #[serde(borrow)]
    signature: Str<'a>,
    #[serde(borrow)]
    partial_json: Str<'a>,
}

fn parse_lines(lines: &mut Lines<impl Read>, on: &mut dyn FnMut(Delta) -> bool) -> Result<Reply> {
    let mut blocks: Vec<Value> = Vec::new();
    let mut partial: Vec<String> = Vec::new();
    let mut usage = Usage::default();
    let mut input_known = false;
    let mut output_known = false;
    let mut stop: Option<String> = None;
    let mut interrupted = false;
    let mut saw_data = false;
    let mut raw = String::new();
    while let Some(line) = lines.next_line() {
        let line = line.context("stream interrupted")?;
        let Some(data) = line.strip_prefix("data:") else {
            if !saw_data && !line.starts_with("event:") {
                raw.push_str(line);
            }
            continue;
        };
        saw_data = true;
        let data = data.trim();
        let mut spare = None;
        let Some(ev) = crate::sse::event::<Event>(data, &mut spare) else { continue };
        if ev.kind.get() == Some("content_block_delta") {
            let i = ev.index.0.unwrap_or(0) as usize;
            let Some(block) = blocks.get_mut(i) else { continue };
            let d = &ev.delta.0;
            let keep_going = match d.kind.get().unwrap_or("") {
                "text_delta" => {
                    let s = d.text.get().unwrap_or("");
                    append(block, "text", s);
                    s.is_empty() || on(Delta::Content(s))
                }
                "thinking_delta" => {
                    let s = d.thinking.get().unwrap_or("");
                    append(block, "thinking", s);
                    s.is_empty() || on(Delta::Reasoning(s))
                }
                "signature_delta" => {
                    append(block, "signature", d.signature.get().unwrap_or(""));
                    true
                }
                "input_json_delta" => {
                    partial[i].push_str(d.partial_json.get().unwrap_or(""));
                    true
                }
                _ => true,
            };
            if !keep_going {
                interrupted = true;
                break;
            }
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        match v["type"].as_str().unwrap_or("") {
            "message_start" => {
                let u = &v["message"]["usage"];
                if let Some(n) = input_usage(u) {
                    usage.prompt = n;
                    (usage.cache_read, usage.cache_write) = cache_usage(u);
                    input_known = true;
                }
                usage.completion = u["output_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let i = v["index"].as_u64().unwrap_or(blocks.len() as u64) as usize;
                while blocks.len() <= i {
                    blocks.push(Value::Null);
                    partial.push(String::new());
                }
                blocks[i] = v["content_block"].clone();
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    stop = Some(s.to_string());
                }
                if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                    usage.completion = n;
                    output_known = true;
                }
            }
            "error" => bail!("API error: {}", v["error"]),
            _ => {}
        }
    }
    if !saw_data && !raw.trim().is_empty() {
        let v: Value = serde_json::from_str(&raw).with_context(|| format!("unexpected response: {raw:.300}"))?;
        if v.get("error").is_some() {
            bail!("API error: {}", v["error"]);
        }
        blocks = v["content"].as_array().cloned().unwrap_or_default();
        partial = vec![String::new(); blocks.len()];
        stop = v["stop_reason"].as_str().map(String::from);
        if let Some(n) = v["usage"]["output_tokens"].as_u64() {
            usage.completion = n;
            output_known = true;
        }
        if let Some(n) = input_usage(&v["usage"]) {
            usage.prompt = n;
            (usage.cache_read, usage.cache_write) = cache_usage(&v["usage"]);
            input_known = true;
        }
        let text: String = blocks.iter().filter_map(|b| b["text"].as_str()).collect();
        if !text.is_empty() {
            let _ = on(Delta::Content(&text));
        }
    }
    let known = input_known && output_known && stop.is_some() && !interrupted;
    Ok(finish(blocks, partial, stop, known.then_some(usage), interrupted))
}

fn input_usage(u: &Value) -> Option<u64> {
    let mut total = u["input_tokens"].as_u64()?;
    for k in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
        if let Some(v) = u.get(k) {
            total = total.checked_add(v.as_u64()?)?;
        }
    }
    Some(total)
}

/// Cache reads and writes, already counted in `input_usage`'s total.
fn cache_usage(u: &Value) -> (u64, u64) {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    (n("cache_read_input_tokens"), n("cache_creation_input_tokens"))
}

/// Grows the block's text in place. Copying it out and back on every delta
/// made a long answer quadratic: minutes and gigabytes for a few megabytes.
fn append(block: &mut Value, key: &str, s: &str) {
    match &mut block[key] {
        Value::String(cur) => cur.push_str(s),
        other => *other = Value::String(s.to_owned()),
    }
}

fn finish(
    mut blocks: Vec<Value>,
    partial: Vec<String>,
    stop: Option<String>,
    usage: Option<Usage>,
    interrupted: bool,
) -> Reply {
    for (b, json_text) in blocks.iter_mut().zip(&partial) {
        if b["type"] == "tool_use" && !json_text.is_empty() {
            b["input"] = serde_json::from_str(json_text).unwrap_or_else(|_| json!({}));
            b["_partial"] = json!(json_text);
        }
    }
    blocks.retain(|b| !b.is_null());
    // After a mid-stream fallback only the text before the last boundary is
    // context; the declined model's thinking and tool calls are not echoed.
    if let Some(cut) = blocks.iter().rposition(|b| b["type"] == "fallback") {
        let after = blocks.split_off(cut + 1);
        blocks.pop();
        blocks.retain(|b| b["type"] == "text");
        blocks.extend(after);
    }
    let finish_reason = if interrupted {
        "interrupted".to_string()
    } else {
        match stop.as_deref() {
            Some("tool_use") => "tool_calls",
            Some("max_tokens") | Some("model_context_window_exceeded") => "length",
            Some("refusal") => "refusal",
            Some(_) | None => "stop",
        }
        .to_string()
    };
    let window_full = stop.as_deref() == Some("model_context_window_exceeded");
    let mut reply = Reply { usage, finish_reason: Some(finish_reason.clone()), window_full, ..Default::default() };
    reply.content =
        blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
    reply.tool_calls = blocks
        .iter()
        .filter(|b| b["type"] == "tool_use")
        .enumerate()
        .map(|(i, b)| ToolCall {
            id: b["id"].as_str().map(String::from).unwrap_or_else(|| format!("call_{i}")),
            name: b["name"].as_str().unwrap_or("").to_string(),
            // The text as streamed, so a cut-off input fails the agent's strict parse.
            arguments: b["_partial"].as_str().map(String::from).unwrap_or_else(|| b["input"].to_string()),
        })
        .filter(|c| !c.name.is_empty())
        .collect();
    if finish_reason == "refusal" && reply.content.trim().is_empty() {
        reply.content = "(Claude declined this request. Try rephrasing it or switching models with /model.)".into();
    }
    // Keep the raw blocks only when the turn is whole: an unsigned thinking
    // block (cut off or interrupted) would be rejected if sent back.
    let whole = matches!(finish_reason.as_str(), "stop" | "tool_calls")
        && blocks.iter().all(|b| b["type"] != "thinking" || b["signature"].as_str().is_some_and(|s| !s.is_empty()));
    if whole {
        for b in blocks.iter_mut() {
            if let Some(o) = b.as_object_mut() {
                o.remove("_partial");
            }
        }
        reply.raw = Some(Value::Array(blocks));
        reply.raw_key = BLOCKS_KEY;
    } else {
        reply.tool_calls.retain(|_| finish_reason != "refusal");
    }
    reply
}

/// Messages as an OpenAI-compatible endpoint gets them: without the keys
/// rusty keeps for itself. Serialised straight from history, not copied.
pub struct Public<'a>(pub &'a [Value]);

impl serde::Serialize for Public<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        struct Message<'a>(&'a Value);
        impl serde::Serialize for Message<'_> {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                let Some(o) = self.0.as_object() else { return self.0.serialize(s) };
                let mut map = s.serialize_map(None)?;
                for (k, v) in o.iter().filter(|(k, _)| !k.starts_with('_')) {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
        let mut seq = s.serialize_seq(Some(self.0.len()))?;
        for m in self.0 {
            seq.serialize_element(&Message(m))?;
        }
        seq.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(events: &[Value]) -> String {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
    }

    fn parse(events: &[Value]) -> (Reply, Vec<String>) {
        let mut seen = Vec::new();
        let text = sse(events);
        let reply = parse_lines(&mut Lines::new(text.as_bytes()), &mut |d| {
            seen.push(match d {
                Delta::Content(s) => format!("c:{s}"),
                Delta::Reasoning(s) => format!("r:{s}"),
            });
            true
        })
        .unwrap();
        (reply, seen)
    }

    fn start(i: u64, block: Value) -> Value {
        json!({"type": "content_block_start", "index": i, "content_block": block})
    }
    fn delta(i: u64, d: Value) -> Value {
        json!({"type": "content_block_delta", "index": i, "delta": d})
    }

    #[test]
    fn streams_thinking_text_and_a_tool_call() {
        let (reply, seen) = parse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 1}}}),
            start(0, json!({"type": "thinking", "thinking": ""})),
            delta(0, json!({"type": "thinking_delta", "thinking": "look at main"})),
            delta(0, json!({"type": "signature_delta", "signature": "sig"})),
            start(1, json!({"type": "text", "text": ""})),
            delta(1, json!({"type": "text_delta", "text": "Reading it."})),
            start(2, json!({"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}})),
            delta(2, json!({"type": "input_json_delta", "partial_json": "{\"path\": "})),
            delta(2, json!({"type": "input_json_delta", "partial_json": "\"src/main.rs\"}"})),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
            json!({"type": "message_stop"}),
        ]);
        assert_eq!(seen, vec!["r:look at main", "c:Reading it."]);
        assert_eq!(reply.content, "Reading it.");
        assert_eq!(reply.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].id, "toolu_1");
        assert_eq!(serde_json::from_str::<Value>(&reply.tool_calls[0].arguments).unwrap()["path"], "src/main.rs");
        let u = reply.usage.unwrap();
        assert_eq!((u.prompt, u.completion), (100, 42));
        let raw = reply.raw.unwrap();
        assert_eq!(raw[0]["signature"], "sig");
        assert_eq!(raw[2]["input"]["path"], "src/main.rs");
        assert!(raw[2].get("_partial").is_none());
    }

    #[test]
    fn a_full_window_is_told_apart_from_a_reply_limit() {
        let (reply, _) = parse(&[
            start(0, json!({"type": "text", "text": ""})),
            delta(0, json!({"type": "text_delta", "text": "partial"})),
            json!({"type": "message_delta", "delta": {"stop_reason": "model_context_window_exceeded"}}),
        ]);
        assert_eq!(reply.finish_reason.as_deref(), Some("length"));
        assert!(reply.window_full);
    }

    #[test]
    fn a_cut_off_turn_keeps_no_raw_blocks() {
        let (reply, _) = parse(&[
            start(0, json!({"type": "thinking", "thinking": ""})),
            delta(0, json!({"type": "thinking_delta", "thinking": "hmm"})),
            start(1, json!({"type": "tool_use", "id": "t", "name": "write_file", "input": {}})),
            delta(1, json!({"type": "input_json_delta", "partial_json": "{\"path\": \"a\", \"content\": \"unfini"})),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
        ]);
        assert_eq!(reply.finish_reason.as_deref(), Some("length"));
        assert!(!reply.window_full, "max_tokens is the reply's own limit");
        assert!(reply.raw.is_none());
        assert!(
            serde_json::from_str::<Value>(&reply.tool_calls[0].arguments).is_err(),
            "a cut-off input must not parse"
        );
    }

    #[test]
    fn a_refusal_runs_no_tools_and_says_so() {
        let (reply, _) = parse(&[
            start(0, json!({"type": "tool_use", "id": "t", "name": "bash", "input": {}})),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal"}}),
        ]);
        assert_eq!(reply.finish_reason.as_deref(), Some("refusal"));
        assert!(reply.tool_calls.is_empty());
        assert!(reply.content.contains("declined"));
    }

    #[test]
    fn after_a_fallback_only_earlier_text_is_echoed() {
        let (reply, _) = parse(&[
            start(0, json!({"type": "thinking", "thinking": "", "signature": "a"})),
            start(1, json!({"type": "text", "text": "Partial answer. "})),
            start(
                2,
                json!({"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-4-8"}}),
            ),
            start(3, json!({"type": "text", "text": "Rest."})),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        ]);
        let raw = reply.raw.unwrap();
        let types: Vec<&str> = raw.as_array().unwrap().iter().map(|b| b["type"].as_str().unwrap()).collect();
        assert_eq!(types, vec!["text", "text"]);
        assert_eq!(reply.content, "Partial answer. Rest.");
    }

    #[test]
    fn deltas_grow_a_block_in_place() {
        let mut block = json!({"type": "text", "text": "a"});
        append(&mut block, "text", "b");
        append(&mut block, "signature", "s");
        assert_eq!(block, json!({"type": "text", "text": "ab", "signature": "s"}));
        let mut odd = json!({"type": "text", "text": 7});
        append(&mut odd, "text", "c");
        assert_eq!(odd["text"], "c", "a non-string is replaced, as before");
    }

    #[test]
    fn mid_stream_errors_fail_the_request() {
        let text = sse(&[json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})]);
        let err = parse_lines(&mut Lines::new(text.as_bytes()), &mut |_| true).unwrap_err();
        assert!(err.to_string().contains("overloaded_error"));
    }

    #[test]
    fn converts_history_and_repairs_tool_pairs() {
        let history = vec![
            json!({"role": "system", "content": "be brief"}),
            json!({"role": "user", "content": "fix it"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"id": "a", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}},
                {"id": "b", "type": "function", "function": {"name": "bash", "arguments": "not json"}}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "main.rs"}),
            json!({"role": "user", "content": "advice: run the tests"}),
            json!({"role": "tool", "tool_call_id": "zzz", "content": "orphan"}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": "done", "_anthropic": [{"type": "thinking", "thinking": "", "signature": "s"}, {"type": "text", "text": "done"}]}),
        ];
        let (system, msgs) = convert(&history);
        assert_eq!(system, "be brief");
        assert_eq!(msgs.len(), 4, "{msgs:#?}");
        assert_eq!(msgs[1]["content"][1]["input"], json!({}), "bad arguments become an empty input");
        let results = &msgs[2]["content"];
        assert_eq!(results[0]["tool_use_id"], "b", "a missing result gets a stub");
        assert_eq!(results[1]["tool_use_id"], "a");
        assert_eq!(results[2]["type"], "text");
        assert!(results[3]["text"].as_str().unwrap().starts_with("(tool output)"), "an orphan result is text");
        assert_eq!(msgs[3]["content"][0]["signature"], "s", "Claude's own blocks go back verbatim");
    }

    #[test]
    fn requests_follow_each_model() {
        let tools = json!([{"type": "function", "function": {"name": "bash", "description": "run", "parameters": {"type": "object"}}}]);
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let (body, beta) = request("claude-opus-5-5", &msgs, &tools, ExecutionMode::Careful, true);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
        assert!(body.get("temperature").is_none());
        assert_eq!(beta.as_deref(), Some("thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01"));

        let (body, beta) = request("claude-haiku-4-5", &msgs, &json!([]), ExecutionMode::Vibe, true);
        assert!(body.get("thinking").is_none() && body.get("output_config").is_none() && body.get("tools").is_none());
        assert!(beta.is_none());

        let (body, beta) = request("claude-opus-4-6", &msgs, &json!([]), ExecutionMode::Careful, true);
        assert_eq!(body["output_config"]["effort"], "high", "no xhigh before Opus 4.7");
        assert!(body["thinking"].get("block_binding").is_none() && beta.is_none());

        let (body, beta) = request("claude-opus-5-5", &msgs, &json!([]), ExecutionMode::Standard, false);
        assert!(body.get("fallbacks").is_none() && beta.is_none(), "betas only on Anthropic's own API");
    }

    #[test]
    fn never_sends_blocks_the_api_rejects() {
        let history = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "content": "", "_anthropic": [
                {"type": "thinking", "thinking": "", "signature": "s"}, {"type": "text", "text": "\n\n"},
                {"type": "tool_use", "id": "t", "name": "bash", "input": {}}]}),
            json!({"role": "tool", "tool_call_id": "t", "content": ""}),
        ];
        let (_, msgs) = convert(&history);
        assert_eq!(msgs[1]["content"].as_array().unwrap().len(), 2, "whitespace text dropped");
        assert_eq!(msgs[2]["content"][0]["content"], "(no output)");
        let (body, _) = request("claude-3-5-haiku-latest", &history, &json!([]), ExecutionMode::Careful, true);
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn strips_private_keys_for_other_providers() {
        let m = vec![json!({"role": "assistant", "content": "x", "_anthropic": []}), json!("plain")];
        assert_eq!(serde_json::to_value(Public(&m)).unwrap(), json!([{"role": "assistant", "content": "x"}, "plain"]));
    }
}
