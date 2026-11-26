//! Gateway adapter for generation and native, non-streaming decision resources.
use zcall::server::protocol as generation;
use zconv::{
    api::{self, Document, Endpoint, Schema},
    json::{Limits, Token},
};

pub use generation::{Inspection, StreamTransform};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    OpenAiChat,
    OpenAiResponses,
    AnthropicMessages,
    GeminiGenerateContent,
    OpenAiCompletions,
    GeminiInteractions,
    SystemOne,
    OpenAiDecisions,
}

impl Protocol {
    fn decision(self) -> Option<Endpoint> {
        match self {
            Self::SystemOne => Some(Endpoint::SystemOne),
            Self::OpenAiDecisions => Some(Endpoint::OpenAiDecisions),
            _ => None,
        }
    }

    pub fn is_decision(self) -> bool {
        self.decision().is_some()
    }

    fn generation(self) -> Result<generation::Protocol, String> {
        use generation::Protocol as G;
        Ok(match self {
            Self::OpenAiChat => G::OpenAiChat,
            Self::OpenAiResponses => G::OpenAiResponses,
            Self::AnthropicMessages => G::AnthropicMessages,
            Self::GeminiGenerateContent => G::GeminiGenerateContent,
            Self::OpenAiCompletions => G::OpenAiCompletions,
            Self::GeminiInteractions => G::GeminiInteractions,
            Self::SystemOne | Self::OpenAiDecisions => {
                return Err(
                    "decision requests require an upstream using the same decision protocol".into(),
                );
            }
        })
    }
}

pub fn parse_protocol(value: &str) -> Result<Protocol, String> {
    use generation::Protocol as G;
    Ok(match value {
        "systemone" | "system-one" => Protocol::SystemOne,
        "decisions" | "openai-decisions" => Protocol::OpenAiDecisions,
        _ => match generation::parse_protocol(value)? {
            G::OpenAiChat => Protocol::OpenAiChat,
            G::OpenAiResponses => Protocol::OpenAiResponses,
            G::AnthropicMessages => Protocol::AnthropicMessages,
            G::GeminiGenerateContent => Protocol::GeminiGenerateContent,
            G::OpenAiCompletions => Protocol::OpenAiCompletions,
            G::GeminiInteractions => Protocol::GeminiInteractions,
        },
    })
}

pub fn protocol_name(protocol: Protocol) -> &'static str {
    protocol
        .decision()
        .and_then(Endpoint::decision_operation)
        .unwrap_or_else(|| {
            generation::protocol_name(protocol.generation().expect("generation protocol"))
        })
}

fn decision_document<T>(
    schema: Schema,
    body: &[u8],
    op: impl FnOnce(Document<'_, '_>) -> Result<T, String>,
) -> Result<T, String> {
    let limits = Limits {
        max_depth: 64,
        max_input: 16 * 1024 * 1024,
        max_tokens: 131_072,
        max_object_fields: 4096,
    };
    let mut tokens = vec![Token::EMPTY; 128];
    loop {
        match api::parse(schema, body, &mut tokens, limits) {
            Ok(document) => return op(document),
            Err(zconv::Error::WorkspaceTooSmall { needed }) if needed <= limits.max_tokens => {
                tokens.resize(
                    (tokens.len() * 2).max(needed).min(limits.max_tokens),
                    Token::EMPTY,
                );
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

pub fn inspect(
    protocol: Protocol,
    body: &[u8],
    path_model: Option<&str>,
) -> Result<Inspection, String> {
    let Some(resource) = protocol.decision() else {
        return generation::inspect(protocol.generation()?, body, path_model);
    };
    decision_document(
        resource.request_schema().expect("decision request schema"),
        body,
        |document| {
            if document
                .field("stream")
                .is_some_and(|stream| stream.as_bool() != Some(false))
            {
                return Err("decision endpoints do not support streaming".into());
            }
            let model = document
                .model()
                .ok_or("specify a gateway model or virtual model")?;
            let size = model.decoded_len();
            if !(1..=zcalld_plugin_api::MAX_MODEL_BYTES).contains(&size) {
                return Err("invalid decision model length".into());
            }
            let mut buffer = vec![0; size];
            model
                .decode_into(&mut buffer)
                .map_err(|error| error.to_string())?;
            let model = String::from_utf8(buffer).map_err(|_| "invalid model UTF-8")?;
            if model.chars().any(char::is_control) {
                return Err("invalid decision model".into());
            }
            Ok(Inspection {
                model,
                stream: false,
            })
        },
    )
}

pub fn endpoint(
    base: &str,
    protocol: Protocol,
    model: &str,
    stream: bool,
) -> Result<String, String> {
    let Some(resource) = protocol.decision() else {
        return generation::endpoint(base, protocol.generation()?, model, stream);
    };
    if stream {
        return Err("decision endpoints do not support streaming".into());
    }
    let mut output = vec![0; base.len().saturating_add(32)];
    zcall::v1::HttpEndpoint::decision_url_into(base, resource, &mut output)
        .map(str::to_owned)
        .map_err(|error| error.to_string())
}

pub fn prepare_request(
    source: Protocol,
    target: Protocol,
    body: &[u8],
    model: &str,
    stream: bool,
    max_tokens: u64,
) -> Result<Vec<u8>, String> {
    if source == target
        && let Some(resource) = source.decision()
    {
        if stream {
            return Err("decision endpoints do not support streaming".into());
        }
        return decision_document(
            resource.request_schema().expect("decision request schema"),
            body,
            |document| {
                let mut output = vec![
                    0;
                    body.len()
                        .saturating_add(model.len().saturating_mul(6))
                        .saturating_add(20)
                ];
                let written = document
                    .encode_decision_model(model, &mut output)
                    .map_err(|error| error.to_string())?;
                output.truncate(written);
                Ok(output)
            },
        );
    }
    generation::prepare_request(
        source.generation()?,
        target.generation()?,
        body,
        model,
        stream,
        max_tokens,
    )
}

pub fn convert_response(
    source: Protocol,
    target: Protocol,
    body: &[u8],
    model: &str,
) -> Result<Vec<u8>, String> {
    if source == target && source.is_decision() {
        return Ok(body.to_vec());
    }
    generation::convert_response(source.generation()?, target.generation()?, body, model)
}

pub fn with_stream_converter<T>(
    source: Protocol,
    target: Protocol,
    model: &str,
    run: impl FnOnce(&mut StreamTransform<'_>) -> T,
) -> Result<T, String> {
    generation::with_stream_converter(source.generation()?, target.generation()?, model, run)
}
