//! A conservative observer of the delivered protocol. It never edits output.
use std::collections::BTreeMap;
use zcalld_compat_client::json::{self, Value};
use zconv::{
    Format, Protocol,
    json::{Limits, Token},
    stream::{self, BlockKind, Completion, Event, EventKind, FinishReason},
};

const FRAME_LIMIT: usize = 256 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;
const TOKEN_LIMIT: usize = 8192;
const EVENT_LIMIT: usize = 1024;

pub fn protocol(id: u32) -> Option<Protocol> {
    Some(match id {
        1 => Protocol::OpenAiChat,
        2 => Protocol::OpenAiResponses,
        3 => Protocol::AnthropicMessages,
        4 => Protocol::GeminiGenerateContent,
        5 => Protocol::OpenAiCompletions,
        6 => Protocol::GeminiInteractions,
        _ => return None,
    })
}
pub struct Scan {
    protocol: Option<Protocol>,
    sse: bool,
    head: bool,
    bad: bool,
    tools: bool,
    finish: bool,
    cr: bool,
    first_line: bool,
    choices: BTreeMap<u32, bool>,
    line: Vec<u8>,
    data: Vec<u8>,
    name: Vec<u8>,
    document: Vec<u8>,
    tokens: Vec<Token>,
    events: Vec<Event>,
}
impl Scan {
    pub fn new(id: u32) -> Self {
        Self {
            protocol: protocol(id),
            sse: false,
            head: false,
            bad: false,
            tools: false,
            finish: false,
            cr: false,
            first_line: true,
            choices: BTreeMap::new(),
            line: Vec::new(),
            data: Vec::new(),
            name: Vec::new(),
            document: Vec::new(),
            tokens: Vec::new(),
            events: Vec::new(),
        }
    }
    pub fn head(&mut self, id: u32, status: u64, streaming: bool, content_type: &str) {
        if self.head || protocol(id) != self.protocol || !(200..300).contains(&status) {
            self.bad = true;
            return;
        }
        self.head = true;
        let mime = content_type.split(';').next().unwrap_or("").trim();
        self.sse = mime.eq_ignore_ascii_case("text/event-stream");
        if streaming && !self.sse || !self.sse && !mime.eq_ignore_ascii_case("application/json") {
            self.bad = true;
        }
    }
    pub fn push(&mut self, bytes: &[u8]) {
        if self.bad || !self.head {
            self.bad = true;
            return;
        }
        if !self.sse {
            if self.document.len().saturating_add(bytes.len()) > JSON_LIMIT {
                self.bad = true;
                self.document.clear();
                return;
            }
            self.document.extend_from_slice(bytes);
            return;
        }
        for &b in bytes {
            if b == b'\n' && self.cr {
                self.cr = false;
                continue;
            }
            if b == b'\n' || b == b'\r' {
                self.cr = b == b'\r';
                let line = std::mem::take(&mut self.line);
                let content = if self.first_line && line.starts_with(b"\xef\xbb\xbf") {
                    &line[3..]
                } else {
                    &line
                };
                self.first_line = false;
                self.line(content);
                self.line = line;
                self.line.clear();
                if self.bad {
                    break;
                }
            } else {
                self.cr = false;
                if self.line.len() >= FRAME_LIMIT {
                    self.bad = true;
                    break;
                }
                self.line.push(b);
            }
        }
    }
    fn line(&mut self, line: &[u8]) {
        if line.is_empty() {
            self.frame();
            return;
        }
        if line[0] == b':' {
            return;
        }
        let (key, mut val) = line
            .iter()
            .position(|b| *b == b':')
            .map_or((line, &[][..]), |i| (&line[..i], &line[i + 1..]));
        if val.first() == Some(&b' ') {
            val = &val[1..];
        }
        match key {
            b"data" => {
                if self.data.len().saturating_add(val.len()).saturating_add(1) > FRAME_LIMIT {
                    self.bad = true;
                    return;
                }
                if !self.data.is_empty() {
                    self.data.push(b'\n');
                }
                self.data.extend_from_slice(val);
            }
            b"event" => {
                if val.len() > 128 {
                    self.bad = true;
                    return;
                }
                self.name.clear();
                self.name.extend_from_slice(val);
            }
            b"id" | b"retry" => {}
            _ => {} // Unknown SSE fields have no semantic meaning.
        }
    }
    fn frame(&mut self) {
        if self.data.is_empty() {
            self.name.clear();
            return;
        }
        let Some(p) = self.protocol else {
            self.bad = true;
            return;
        };
        if self.finish {
            self.bad = true;
            return;
        }
        if self.tokens.is_empty() {
            self.tokens.resize(TOKEN_LIMIT, Token::EMPTY);
            self.events.resize(EVENT_LIMIT, Event::EMPTY);
        }
        let name = if self.name.is_empty() {
            b"message".as_slice()
        } else {
            &self.name
        };
        let n = match stream::decode_frame_format(
            Format::new(p),
            name,
            &self.data,
            &mut self.tokens,
            &mut self.events,
            Limits {
                max_input: FRAME_LIMIT,
                ..Limits::default()
            },
        ) {
            Ok(n) => n,
            Err(_) => {
                self.bad = true;
                return;
            }
        };
        for i in 0..n {
            let e = self.events[i];
            if matches!(
                e.kind,
                EventKind::BlockStart | EventKind::Delta | EventKind::ToolComplete
            ) && e.block == BlockKind::Tool
            {
                self.tools = true;
            }
            match e.kind {
                EventKind::BlockStart | EventKind::Delta | EventKind::ToolComplete => {
                    if self.choices.len() >= 16 && !self.choices.contains_key(&e.key.choice) {
                        self.bad = true;
                        break;
                    }
                    self.choices.entry(e.key.choice).or_insert(false);
                }
                EventKind::ChoiceEnd => {
                    if self.choices.len() >= 16 && !self.choices.contains_key(&e.key.choice) {
                        self.bad = true;
                        break;
                    }
                    if e.reason != FinishReason::Stop {
                        self.bad = true;
                    }
                    self.choices
                        .insert(e.key.choice, e.reason == FinishReason::Stop);
                }
                EventKind::Finish => {
                    self.finish = e.completion == Completion::Completed;
                    if !self.finish {
                        self.bad = true;
                    }
                }
                EventKind::Failure | EventKind::BlockIncomplete => self.bad = true,
                _ => {}
            }
        }
        self.data.clear();
        self.name.clear();
    }
    pub fn completed(&mut self) -> bool {
        if self.bad || !self.head || self.tools || self.protocol.is_none() {
            return false;
        }
        if self.sse {
            // An unterminated final SSE frame is not proof of completion.
            if !self.line.is_empty() || !self.data.is_empty() {
                return false;
            }
            (self.finish || self.protocol == Some(Protocol::GeminiGenerateContent))
                && !self.choices.is_empty()
                && self.choices.values().all(|b| *b)
        } else {
            let Ok(v) = json::parse(&self.document) else {
                return false;
            };
            complete_json(self.protocol.unwrap(), &v)
        }
    }
}

fn list(v: &Value) -> Option<&[Value]> {
    if let Value::Array(v) = v {
        Some(v)
    } else {
        None
    }
}
fn absent(v: &Value) -> bool {
    matches!(v, Value::Null) || matches!(v,Value::Array(v) if v.is_empty())
}
fn safe_outputs(v: &Value) -> bool {
    let Some(items) = list(v) else {
        return false;
    };
    items.iter().all(|v| {
        matches!(
            v.get("type").as_str(),
            Some("message" | "reasoning" | "compaction" | "text" | "thought" | "model_output")
        )
    })
}
fn complete_json(p: Protocol, v: &Value) -> bool {
    match p {
        Protocol::OpenAiChat | Protocol::OpenAiCompletions => {
            list(v.get("choices")).is_some_and(|c| {
                !c.is_empty()
                    && c.len() <= 16
                    && c.iter().all(|c| {
                        c.get("finish_reason").as_str() == Some("stop")
                            && absent(c.get("message").get("tool_calls"))
                            && absent(c.get("message").get("function_call"))
                    })
            })
        }
        Protocol::OpenAiResponses => {
            v.get("status").as_str() == Some("completed")
                && absent(v.get("error"))
                && safe_outputs(v.get("output"))
        }
        Protocol::AnthropicMessages => {
            matches!(
                v.get("stop_reason").as_str(),
                Some("end_turn" | "stop_sequence")
            ) && list(v.get("content")).is_some_and(|a| {
                a.iter().all(|x| {
                    matches!(
                        x.get("type").as_str(),
                        Some("text" | "thinking" | "redacted_thinking")
                    )
                })
            })
        }
        Protocol::GeminiGenerateContent => list(v.get("candidates")).is_some_and(|a| {
            !a.is_empty()
                && a.len() <= 16
                && a.iter().all(|x| {
                    x.get("finishReason").as_str() == Some("STOP")
                        && list(x.get("content").get("parts"))
                            .is_some_and(|p| p.iter().all(|p| absent(p.get("functionCall"))))
                })
        }),
        Protocol::GeminiInteractions => {
            v.get("status").as_str() == Some("completed") && safe_outputs(v.get("outputs"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sse(protocol: u32, body: &[u8]) -> bool {
        let mut s = Scan::new(protocol);
        s.head(protocol, 200, true, "text/event-stream");
        for c in body.chunks(3) {
            s.push(c);
        }
        s.completed()
    }
    #[test]
    fn fragmented_chat_requires_terminal_reason_and_done_not_quoted_text() {
        let good=b"data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"tool_calls length [DONE]\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        assert!(sse(1, good));
        assert!(!sse(1, &good[..good.len() - 16]));
        assert!(!sse(
            1,
            &String::from_utf8_lossy(good)
                .replace("\"stop\"", "\"length\"")
                .into_bytes()
        ));
    }
    #[test]
    fn explicit_tool_and_failed_response_never_finish() {
        assert!(!sse(1,b"data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"t\",\"type\":\"function\",\"function\":{\"name\":\"f\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"));
        assert!(!sse(2,b"event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"test\"}}}\n\n"));
    }
    #[test]
    fn nonstream_protocols_require_proven_success() {
        for (p, body) in [
            (
                1,
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"done"}}]}"#,
            ),
            (
                2,
                r#"{"status":"completed","output":[{"type":"message","content":[]}]}"#,
            ),
            (
                3,
                r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}"#,
            ),
            (
                4,
                r#"{"candidates":[{"finishReason":"STOP","content":{"parts":[{"text":"ok"}]}}]}"#,
            ),
            (5, r#"{"choices":[{"finish_reason":"stop","text":"ok"}]}"#),
            (
                6,
                r#"{"status":"completed","outputs":[{"type":"text","text":"ok"}]}"#,
            ),
        ] {
            let mut s = Scan::new(p);
            s.head(p, 200, false, "application/json");
            s.push(body.as_bytes());
            assert!(s.completed(), "{p}");
        }
        for body in [
            r#"{"status":"incomplete","output":[]}"#,
            r#"{"status":"completed","output":[{"type":"function_call"}]}"#,
        ] {
            let mut s = Scan::new(2);
            s.head(2, 200, false, "application/json");
            s.push(body.as_bytes());
            assert!(!s.completed());
        }
    }
    #[test]
    fn capacity_and_unknown_protocol_only_disable_observer() {
        let mut s = Scan::new(1);
        s.head(1, 200, true, "text/event-stream");
        s.push(&vec![b'a'; FRAME_LIMIT + 1]);
        assert!(!s.completed());
        let mut s = Scan::new(99);
        s.head(99, 200, false, "application/json");
        s.push(b"{}");
        assert!(!s.completed());
    }
}
