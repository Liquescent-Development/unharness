//! Minimal JSON-RPC 2.0 framing for stdio protocols (Codex app-server, ACP).

use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum RpcMessage {
    /// Server → client request; must be answered with `id`.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<Value>,
    },
}

impl RpcMessage {
    pub fn parse(line: &str) -> Option<RpcMessage> {
        let v: Value = serde_json::from_str(line).ok()?;
        Self::from_value(v)
    }

    pub fn from_value(v: Value) -> Option<RpcMessage> {
        let method = v.get("method").and_then(Value::as_str).map(str::to_string);
        let id = v.get("id").cloned();
        match (method, id) {
            (Some(method), Some(id)) if !id.is_null() => Some(RpcMessage::Request {
                id,
                method,
                params: v.get("params").cloned().unwrap_or(Value::Null),
            }),
            (Some(method), _) => Some(RpcMessage::Notification {
                method,
                params: v.get("params").cloned().unwrap_or(Value::Null),
            }),
            (None, Some(id)) => Some(RpcMessage::Response {
                id,
                result: v.get("result").cloned(),
                error: v.get("error").cloned(),
            }),
            (None, None) => None,
        }
    }
}

/// The id a client request of ours went under: numbered, or a name of its
/// own (Codex's `skills/list`, so that the numbered ones keep theirs).
/// JSON-RPC tells `1` and `"1"` apart, and so does this.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RequestId {
    Number(u64),
    Name(String),
}

impl RequestId {
    /// The id of an answer, if it is one a request of ours can have had.
    pub fn of(id: &Value) -> Option<RequestId> {
        match id {
            Value::Number(n) => n.as_u64().map(RequestId::Number),
            Value::String(s) => Some(RequestId::Name(s.clone())),
            _ => None,
        }
    }
}

impl From<&RequestId> for Value {
    fn from(id: &RequestId) -> Value {
        match id {
            RequestId::Number(n) => Value::from(*n),
            RequestId::Name(s) => Value::from(s.as_str()),
        }
    }
}

/// A request under `id`: a number, or a name (`"skills"`).
pub fn request(id: impl Into<Value>, method: &str, params: Value) -> String {
    let id: Value = id.into();
    let mut m = json!({"jsonrpc":"2.0","id":id,"method":method});
    if !params.is_null() {
        m["params"] = params;
    }
    m.to_string()
}

pub fn notification(method: &str, params: Value) -> String {
    let mut m = json!({"jsonrpc":"2.0","method":method});
    if !params.is_null() {
        m["params"] = params;
    }
    m.to_string()
}

pub fn response(id: &Value, result: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

pub fn error_response(id: &Value, code: i64, message: &str) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_messages() {
        assert_eq!(
            RpcMessage::parse(r#"{"method":"x/y","id":0,"params":{"a":1}}"#),
            Some(RpcMessage::Request {
                id: json!(0),
                method: "x/y".into(),
                params: json!({"a":1})
            })
        );
        assert_eq!(
            RpcMessage::parse(r#"{"method":"n","params":null}"#),
            Some(RpcMessage::Notification {
                method: "n".into(),
                params: Value::Null
            })
        );
        assert_eq!(
            RpcMessage::parse(r#"{"id":"abc","result":{"ok":true}}"#),
            Some(RpcMessage::Response {
                id: json!("abc"),
                result: Some(json!({"ok":true})),
                error: None
            })
        );
        assert_eq!(RpcMessage::parse(r#"{"foo":1}"#), None);
        assert_eq!(RpcMessage::parse("not json"), None);
    }

    #[test]
    fn encode() {
        let v: Value = serde_json::from_str(&request(7, "m", json!({"k":"v"}))).unwrap();
        assert_eq!(
            v,
            json!({"jsonrpc":"2.0","id":7,"method":"m","params":{"k":"v"}})
        );
        let v: Value = serde_json::from_str(&request("skills", "skills/list", json!({}))).unwrap();
        assert_eq!(v["id"], "skills");
        let v: Value = serde_json::from_str(&notification("initialized", Value::Null)).unwrap();
        assert_eq!(v, json!({"jsonrpc":"2.0","method":"initialized"}));
        let v: Value =
            serde_json::from_str(&response(&json!(0), json!({"decision":"accept"}))).unwrap();
        assert_eq!(v["id"], 0);
        let v: Value = serde_json::from_str(&error_response(&json!("x"), -32601, "nope")).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }

    #[test]
    fn request_ids() {
        assert_eq!(RequestId::of(&json!(3)), Some(RequestId::Number(3)));
        assert_eq!(
            RequestId::of(&json!("3")),
            Some(RequestId::Name("3".into()))
        );
        for never_ours in [json!(-1), json!(1.5), Value::Null, json!([1])] {
            assert_eq!(RequestId::of(&never_ours), None, "{never_ours}");
        }
        for id in [RequestId::Number(3), RequestId::Name("skills".into())] {
            assert_eq!(RequestId::of(&Value::from(&id)), Some(id));
        }
    }
}
