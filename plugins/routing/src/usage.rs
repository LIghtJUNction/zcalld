//! Incremental usage observation. Response text is never retained or parsed.
use crate::json::{self, Value};

const MAX_USAGE_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 128;
const MAX_KEY_BYTES: usize = 128;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
}

fn first(values: &[&Value]) -> u64 {
    values.iter().find_map(|value| value.as_u64()).unwrap_or(0)
}

fn merge(usage: &Value, tokens: &mut Tokens) {
    tokens.input = tokens.input.max(first(&[
        usage.get("input_tokens"),
        usage.get("prompt_tokens"),
        usage.get("promptTokenCount"),
    ]));
    tokens.output = tokens.output.max(first(&[
        usage.get("output_tokens"),
        usage.get("completion_tokens"),
        usage.get("candidatesTokenCount"),
    ]));
    tokens.cache_read = tokens.cache_read.max(first(&[
        usage.get("cache_read_input_tokens"),
        usage.get("input_tokens_details").get("cached_tokens"),
        usage.get("prompt_tokens_details").get("cached_tokens"),
        usage.get("cachedContentTokenCount"),
    ]));
    let creation = usage.get("cache_creation");
    let cache_write = usage
        .get("cache_creation_input_tokens")
        .as_u64()
        .unwrap_or_else(|| {
            first(&[creation.get("ephemeral_5m_input_tokens")])
                .saturating_add(first(&[creation.get("ephemeral_1h_input_tokens")]))
        });
    tokens.cache_write = tokens.cache_write.max(cache_write);
    tokens.reasoning = tokens.reasoning.max(first(&[
        usage.get("output_tokens_details").get("reasoning_tokens"),
        usage
            .get("completion_tokens_details")
            .get("reasoning_tokens"),
        usage.get("reasoning_tokens"),
        usage.get("thoughtsTokenCount"),
    ]));
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Field {
    #[default]
    Other,
    Usage,
    UsageMetadata,
    Message,
    Response,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    Root,
    Envelope,
    Other,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Key,
    Colon,
    Value,
    After,
}

struct Frame {
    object: bool,
    place: Place,
    phase: Phase,
    field: Field,
}

#[derive(Default)]
struct Scanner {
    frames: Vec<Frame>,
    started: bool,
    disabled: bool,
    string: bool,
    escape: bool,
    key_string: bool,
    key: Vec<u8>,
    key_overflow: bool,
    capture_depth: Option<usize>,
    capture: Vec<u8>,
    capture_overflow: bool,
}

impl Scanner {
    fn reset(&mut self) {
        self.frames.clear();
        self.started = false;
        self.disabled = false;
        self.string = false;
        self.escape = false;
        self.key_string = false;
        self.key.clear();
        self.key_overflow = false;
        self.capture_depth = None;
        self.capture.clear();
        self.capture_overflow = false;
    }

    fn open(&mut self, object: bool, place: Place) {
        if self.frames.len() == MAX_DEPTH {
            self.disabled = true;
            return;
        }
        self.frames.push(Frame {
            object,
            place,
            phase: if object { Phase::Key } else { Phase::Value },
            field: Field::Other,
        });
    }

    fn close(&mut self, tokens: &mut Tokens) {
        if self.capture_depth == Some(self.frames.len()) {
            if !self.capture_overflow
                && let Ok(value) = json::parse(&self.capture)
            {
                merge(&value, tokens);
            }
            self.capture_depth = None;
            self.capture.clear();
        }
        self.frames.pop();
    }

    fn byte(&mut self, byte: u8, tokens: &mut Tokens) {
        if self.disabled {
            return;
        }
        if self.capture_depth.is_some() {
            if self.capture.len() < MAX_USAGE_BYTES {
                self.capture.push(byte);
            } else {
                self.capture_overflow = true;
            }
        }
        if self.string {
            if self.key_string {
                if self.key.len() < MAX_KEY_BYTES {
                    self.key.push(byte);
                } else {
                    self.key_overflow = true;
                }
            }
            if self.escape {
                self.escape = false;
            } else if byte == b'\\' {
                self.escape = true;
            } else if byte == b'"' {
                self.string = false;
                if self.key_string {
                    let field = if self.key_overflow {
                        Field::Other
                    } else {
                        match self.key.as_slice() {
                            b"\"usage\"" => Field::Usage,
                            b"\"usageMetadata\"" => Field::UsageMetadata,
                            b"\"message\"" => Field::Message,
                            b"\"response\"" => Field::Response,
                            _ if self.key.contains(&b'\\') => {
                                match json::parse(&self.key).ok().as_ref().and_then(Value::as_str) {
                                    Some("usage") => Field::Usage,
                                    Some("usageMetadata") => Field::UsageMetadata,
                                    Some("message") => Field::Message,
                                    Some("response") => Field::Response,
                                    _ => Field::Other,
                                }
                            }
                            _ => Field::Other,
                        }
                    };
                    if let Some(frame) = self.frames.last_mut() {
                        frame.field = field;
                        frame.phase = Phase::Colon;
                    }
                }
            }
            return;
        }
        if byte.is_ascii_whitespace() {
            return;
        }
        let Some(frame) = self.frames.last_mut() else {
            if !self.started {
                self.started = true;
                if byte == b'{' {
                    self.open(true, Place::Root);
                } else {
                    self.disabled = true;
                }
            }
            return;
        };
        match frame.phase {
            Phase::Key if byte == b'"' => {
                self.string = true;
                self.key_string = true;
                self.key.clear();
                self.key.push(b'"');
                self.key_overflow = false;
            }
            Phase::Key if byte == b'}' => self.close(tokens),
            Phase::Colon if byte == b':' => frame.phase = Phase::Value,
            Phase::Value if !frame.object && byte == b']' => self.close(tokens),
            Phase::Value => {
                let place = if frame.place == Place::Root
                    && matches!(frame.field, Field::Message | Field::Response)
                {
                    Place::Envelope
                } else {
                    Place::Other
                };
                let usage = matches!(frame.place, Place::Root | Place::Envelope)
                    && matches!(frame.field, Field::Usage | Field::UsageMetadata);
                frame.phase = Phase::After;
                match byte {
                    b'{' => {
                        if usage && self.capture_depth.is_none() {
                            self.capture.clear();
                            self.capture.push(byte);
                            self.capture_depth = Some(self.frames.len() + 1);
                            self.capture_overflow = false;
                        }
                        self.open(true, place);
                    }
                    b'[' => self.open(false, Place::Other),
                    b'"' => {
                        self.string = true;
                        self.key_string = false;
                    }
                    b'-' | b'0'..=b'9' | b't' | b'f' | b'n' => {}
                    _ => self.disabled = true,
                }
            }
            Phase::After => match byte {
                b',' => {
                    frame.phase = if frame.object {
                        Phase::Key
                    } else {
                        Phase::Value
                    };
                    frame.field = Field::Other;
                }
                b'}' if frame.object => self.close(tokens),
                b']' if !frame.object => self.close(tokens),
                // Scalars outside the captured usage object need no storage.
                b'0'..=b'9'
                | b'.'
                | b'-'
                | b'+'
                | b'e'
                | b'E'
                | b'a'
                | b'l'
                | b's'
                | b't'
                | b'r'
                | b'u' => {}
                _ => self.disabled = true,
            },
            _ => self.disabled = true,
        }
    }
}

#[derive(Default)]
pub struct Collector {
    pub tokens: Tokens,
    scanner: Scanner,
    stream: bool,
    prefix: [u8; 5],
    prefix_len: usize,
    data: bool,
    first: bool,
    after_cr: bool,
}

impl Collector {
    pub fn reset(&mut self, stream: bool) {
        self.tokens = Tokens::default();
        self.scanner.reset();
        self.stream = stream;
        self.prefix_len = 0;
        self.data = false;
        self.first = false;
        self.after_cr = false;
    }

    pub fn write(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if !self.stream {
                self.scanner.byte(byte, &mut self.tokens);
                continue;
            }
            if self.after_cr && byte == b'\n' {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                if self.prefix_len == 0 {
                    self.scanner.reset();
                } else if self.data {
                    // SSE joins successive data fields with a newline.
                    self.scanner.byte(b'\n', &mut self.tokens);
                }
                self.prefix_len = 0;
                self.data = false;
                continue;
            }
            if self.prefix_len < self.prefix.len() {
                self.prefix[self.prefix_len] = byte;
                self.prefix_len += 1;
                if self.prefix_len == self.prefix.len() {
                    self.data = &self.prefix == b"data:";
                    self.first = self.data;
                }
            } else if self.data {
                if self.first {
                    self.first = false;
                    if byte == b' ' {
                        continue;
                    }
                }
                self.scanner.byte(byte, &mut self.tokens);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(input: &[u8], stream: bool, chunk: usize) -> Collector {
        let mut collector = Collector::default();
        collector.reset(stream);
        for bytes in input.chunks(chunk) {
            collector.write(bytes);
        }
        collector
    }

    #[test]
    fn protocol_usage_and_details_survive_every_chunk_boundary() {
        let cases = [
            (
                r#"{"usage":{"prompt_tokens":19,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":11},"completion_tokens_details":{"reasoning_tokens":3}}}"#,
                Tokens {
                    input: 19,
                    output: 7,
                    cache_read: 11,
                    reasoning: 3,
                    ..Tokens::default()
                },
            ),
            (
                r#"{"response":{"usage":{"input_tokens":19,"output_tokens":7,"input_tokens_details":{"cached_tokens":11},"output_tokens_details":{"reasoning_tokens":3}}}}"#,
                Tokens {
                    input: 19,
                    output: 7,
                    cache_read: 11,
                    reasoning: 3,
                    ..Tokens::default()
                },
            ),
            (
                r#"{"message":{"usage":{"input_tokens":19,"output_tokens":7,"cache_read_input_tokens":11,"cache_creation_input_tokens":5}}}"#,
                Tokens {
                    input: 19,
                    output: 7,
                    cache_read: 11,
                    cache_write: 5,
                    ..Tokens::default()
                },
            ),
            (
                r#"{"usageMetadata":{"promptTokenCount":19,"candidatesTokenCount":7,"cachedContentTokenCount":11,"thoughtsTokenCount":3}}"#,
                Tokens {
                    input: 19,
                    output: 7,
                    cache_read: 11,
                    reasoning: 3,
                    ..Tokens::default()
                },
            ),
        ];
        for (value, expected) in cases {
            for chunk in [1, 3, 17, 4096] {
                assert_eq!(collect(value.as_bytes(), false, chunk).tokens, expected);
                let event = format!("event: reply\r\ndata:{value}\r\n\r\ndata: [DONE]\r\n\r\n");
                assert_eq!(collect(event.as_bytes(), true, chunk).tokens, expected);
            }
        }
    }

    #[test]
    fn anthropic_start_and_delta_merge_cumulative_totals_once() {
        let events = b"data:{\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":19,\"cache_read_input_tokens\":11,\"cache_creation_input_tokens\":5}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\ndata: {\"usage\":{\"output_tokens\":7}}\n\n";
        assert_eq!(
            collect(events, true, 1).tokens,
            Tokens {
                input: 19,
                output: 7,
                cache_read: 11,
                cache_write: 5,
                reasoning: 0
            }
        );
    }

    #[test]
    fn long_text_and_sse_events_keep_only_usage() {
        let body = format!(
            r#"{{"choices":[{{"message":{{"content":"{}","usage":{{"prompt_tokens":999}}}}}}],"usage":{{"prompt_tokens":19,"completion_tokens":7}}}}"#,
            "text with \\\"usage\\\" and braces {{ }} ".repeat(32768)
        );
        for stream in [false, true] {
            let input = if stream {
                format!("data:{body}\n\n")
            } else {
                body.clone()
            };
            let collector = collect(input.as_bytes(), stream, 113);
            assert_eq!(collector.tokens.input, 19);
            assert_eq!(collector.tokens.output, 7);
            assert!(collector.scanner.capture.capacity() <= MAX_USAGE_BYTES);
            assert!(collector.scanner.key.capacity() <= MAX_KEY_BYTES);
        }
    }

    #[test]
    fn multiline_sse_and_escaped_usage_key_are_supported() {
        let event = b": heartbeat\ndata: {\"u\\u0073age\":\ndata:{\"input_tokens\":9,\"output_tokens\":3}}\n\n";
        assert_eq!(collect(event, true, 1).tokens.input, 9);
        assert_eq!(collect(event, true, 1).tokens.output, 3);
    }

    #[test]
    fn oversized_usage_is_skipped_and_next_event_recovers() {
        let event = format!(
            "data: {{\"usage\":{{\"padding\":\"{}\",\"input_tokens\":999}}}}\n\ndata: {{\"usage\":{{\"input_tokens\":9}}}}\n\n",
            "x".repeat(MAX_USAGE_BYTES)
        );
        let collector = collect(event.as_bytes(), true, 7);
        assert_eq!(collector.tokens.input, 9);
        assert!(collector.scanner.capture.capacity() <= MAX_USAGE_BYTES);
    }

    #[test]
    fn reset_drops_attempt_totals() {
        let mut collector = collect(b"{\"usage\":{\"input_tokens\":9}}", false, 1);
        collector.reset(false);
        collector.write(b"{\"usage\":{\"output_tokens\":3}}");
        assert_eq!(collector.tokens.input, 0);
        assert_eq!(collector.tokens.output, 3);
    }
}
