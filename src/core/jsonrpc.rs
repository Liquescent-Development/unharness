//! Minimal JSON-RPC 2.0 framing for stdio protocols (codex app-server).

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

pub fn request(id: u64, method: &str, params: Value) -> String {
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
        let v: Value = serde_json::from_str(&notification("initialized", Value::Null)).unwrap();
        assert_eq!(v, json!({"jsonrpc":"2.0","method":"initialized"}));
        let v: Value =
            serde_json::from_str(&response(&json!(0), json!({"decision":"accept"}))).unwrap();
        assert_eq!(v["id"], 0);
        let v: Value = serde_json::from_str(&error_response(&json!("x"), -32601, "nope")).unwrap();
        assert_eq!(v["error"]["code"], -32601);
    }
}
