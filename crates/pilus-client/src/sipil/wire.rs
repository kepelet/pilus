//! The framing every sipil node speaks.
//!
//! One bidirectional QUIC stream carries one call. The caller writes a single
//! JSON object, `{"method", "params"?}`, and finishes the stream; the answer is
//! a single JSON object, `{"result"}` or `{"error": {"code", "message"}}`, and
//! the answerer finishes. Who is calling is never part of the frame: it is the
//! device ID the QUIC handshake proved. The sipil daemon (`../sipil`) speaks the
//! same frames, so a pilus window answers a pilus window exactly as a daemon
//! answers a window.

use serde_json::{Value, json};

/// The ALPN of this protocol.
pub const ALPN: &[u8] = b"pilus/sipil/1";

/// The revision of the protocol. Revision 1 was the daemon's HTTP API, which is
/// gone; revision 3 dropped provider keys and model choice, which the daemon
/// sets on its own machine.
pub const API_VERSION: u32 = 3;

/// The most a request may weigh: a message and a few names.
pub const REQUEST_LIMIT: usize = 1 << 20;
/// The most an answer may weigh: a transcript of the newest 200 messages.
pub const REPLY_LIMIT: usize = 8 << 20;

/// A call refused with a reason a person can read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Failure {
    /// A stable, machine-readable code such as `unauthorized`.
    pub code: String,
    /// Text meant for the person.
    pub message: String,
}

impl Failure {
    /// A failure with `code` and `message`.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// The bytes of a request.
pub fn request(method: &str, params: &Value) -> Vec<u8> {
    json!({ "method": method, "params": params })
        .to_string()
        .into_bytes()
}

/// The bytes of an answer.
pub fn reply(outcome: &Result<Value, Failure>) -> Vec<u8> {
    match outcome {
        Ok(result) => json!({ "result": result }),
        Err(failure) => json!({ "error": { "code": failure.code, "message": failure.message } }),
    }
    .to_string()
    .into_bytes()
}

/// Read a request: a method and its params, which default to an empty object.
pub fn parse_request(bytes: &[u8]) -> Option<(String, Value)> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let method = value.get("method")?.as_str()?.to_string();
    let params = match value.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(params) if params.is_object() => params.clone(),
        Some(_) => return None,
    };
    Some((method, params))
}

/// Read an answer. `None` is a body that is neither a result nor an error.
pub fn parse_reply(bytes: &[u8]) -> Option<Result<Value, Failure>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if let Some(error) = value.get("error") {
        let text = |key: &str| error.get(key).and_then(Value::as_str).map(str::to_string);
        return Some(Err(Failure {
            code: text("code").unwrap_or_else(|| "unknown".to_string()),
            message: text("message").unwrap_or_else(|| "The other side refused.".to_string()),
        }));
    }
    value.get("result").cloned().map(Ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_and_params_default_to_an_object() {
        let (method, params) = parse_request(&request("agents", &json!({"a": 1}))).unwrap();
        assert_eq!((method.as_str(), params), ("agents", json!({"a": 1})));
        let (_, params) = parse_request(br#"{"method":"agents"}"#).unwrap();
        assert_eq!(params, json!({}));
    }

    #[test]
    fn a_request_that_is_not_an_object_with_a_method_is_refused() {
        assert!(parse_request(b"[]").is_none());
        assert!(parse_request(br#"{"method":3}"#).is_none());
        assert!(parse_request(br#"{"method":"x","params":[1]}"#).is_none());
        assert!(parse_request(b"not json").is_none());
    }

    #[test]
    fn an_error_reply_carries_its_code_and_words() {
        let bytes = reply(&Err(Failure::new("unauthorized", "Not you.")));
        assert_eq!(
            parse_reply(&bytes),
            Some(Err(Failure::new("unauthorized", "Not you.")))
        );
        assert_eq!(
            parse_reply(&reply(&Ok(json!({"a": 1})))),
            Some(Ok(json!({"a": 1})))
        );
        assert_eq!(parse_reply(b"{}"), None);
    }
}
