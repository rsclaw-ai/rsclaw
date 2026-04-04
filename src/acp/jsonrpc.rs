//! JSON-RPC 2.0 types for ACP protocol
//!
//! Implements the JSON-RPC 2.0 specification: https://www.jsonrpc.org/specification

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 version string
pub const JSONRPC_VERSION: &str = "2.0";

/// JSON-RPC Request identifier
/// Can be: number, string, or null
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    String(String),
    Null,
}

impl Id {
    /// Check if this is a notification (no ID)
    pub fn is_notification(&self) -> bool {
        matches!(self, Id::Null)
    }
}

impl Default for Id {
    fn default() -> Self {
        Id::Null
    }
}

/// JSON-RPC Error object
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcError {
    /// Error code
    pub code: i32,
    /// Error message
    pub message: String,
    /// Additional error data (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// Create a new JSON-RPC error
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Create an error with data
    pub fn with_data(code: i32, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }

    /// Parse error (Invalid JSON)
    pub const PARSE_ERROR: i32 = -32700;
    /// Invalid request
    pub const INVALID_REQUEST: i32 = -32600;
    /// Method not found
    pub const METHOD_NOT_FOUND: i32 = -32601;
    /// Invalid params
    pub const INVALID_PARAMS: i32 = -32602;
    /// Internal error
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// JSON-RPC Request object
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcRequest {
    /// JSON-RPC version (must be "2.0")
    pub jsonrpc: String,
    /// Request ID
    pub id: Id,
    /// Method name
    pub method: String,
    /// Method parameters (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    /// Create a new request
    pub fn new(id: Id, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }

    /// Create a notification (no ID)
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> Self {
        Self::new(Id::Null, method, params)
    }
}

/// JSON-RPC Response object (success)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcSuccessResponse {
    /// JSON-RPC version (must be "2.0")
    pub jsonrpc: String,
    /// Request ID this response corresponds to
    pub id: Id,
    /// Result value
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

/// JSON-RPC Response object (error)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcErrorResponse {
    /// JSON-RPC version (must be "2.0")
    pub jsonrpc: String,
    /// Request ID this response corresponds to
    pub id: Id,
    /// Error object
    pub error: JsonRpcError,
}

/// JSON-RPC Response (either success or error)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcResponse {
    /// Successful response
    Success(JsonRpcSuccessResponse),
    /// Error response
    Error(JsonRpcErrorResponse),
}

impl JsonRpcResponse {
    /// Create a success response
    pub fn success(id: Id, result: Option<Value>) -> Self {
        Self::Success(JsonRpcSuccessResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        })
    }

    /// Create an error response
    pub fn error(id: Id, error: JsonRpcError) -> Self {
        Self::Error(JsonRpcErrorResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error,
        })
    }
}

/// JSON-RPC Notification (no response expected)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonRpcNotification {
    /// JSON-RPC version (must be "2.0")
    pub jsonrpc: String,
    /// Method name
    pub method: String,
    /// Method parameters (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcNotification {
    /// Create a new notification
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

/// Any JSON-RPC message (request, response, or notification)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcMessage {
    /// Request (expects response)
    Request(JsonRpcRequest),
    /// Success response
    SuccessResponse(JsonRpcSuccessResponse),
    /// Error response
    ErrorResponse(JsonRpcErrorResponse),
    /// Notification (no response expected)
    Notification(JsonRpcNotification),
}

impl JsonRpcMessage {
    /// Check if this is a request
    pub fn is_request(&self) -> bool {
        matches!(self, JsonRpcMessage::Request(_))
    }

    /// Check if this is a response
    pub fn is_response(&self) -> bool {
        matches!(
            self,
            JsonRpcMessage::SuccessResponse(_) | JsonRpcMessage::ErrorResponse(_)
        )
    }

    /// Check if this is a notification
    pub fn is_notification(&self) -> bool {
        matches!(self, JsonRpcMessage::Notification(_))
    }

    /// Get the ID if this is a request or response
    pub fn id(&self) -> Option<&Id> {
        match self {
            JsonRpcMessage::Request(req) => Some(&req.id),
            JsonRpcMessage::SuccessResponse(resp) => Some(&resp.id),
            JsonRpcMessage::ErrorResponse(resp) => Some(&resp.id),
            JsonRpcMessage::Notification(_) => None,
        }
    }

    /// Get the method name if this is a request or notification
    pub fn method(&self) -> Option<&str> {
        match self {
            JsonRpcMessage::Request(req) => Some(&req.method),
            JsonRpcMessage::Notification(notif) => Some(&notif.method),
            _ => None,
        }
    }

    /// Convert to a request if possible
    pub fn into_request(self) -> Option<JsonRpcRequest> {
        match self {
            JsonRpcMessage::Request(req) => Some(req),
            _ => None,
        }
    }

    /// Convert to a notification if possible
    pub fn into_notification(self) -> Option<JsonRpcNotification> {
        match self {
            JsonRpcMessage::Notification(notif) => Some(notif),
            _ => None,
        }
    }

    /// Convert to a response if possible
    pub fn into_response(self) -> Option<JsonRpcResponse> {
        match self {
            JsonRpcMessage::SuccessResponse(resp) => Some(JsonRpcResponse::Success(resp)),
            JsonRpcMessage::ErrorResponse(resp) => Some(JsonRpcResponse::Error(resp)),
            _ => None,
        }
    }
}

/// Builder for creating JSON-RPC messages
pub mod builder {
    use super::*;

    /// Create a new request
    pub fn request(
        id: impl Into<Id>,
        method: impl Into<String>,
        params: Option<Value>,
    ) -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest::new(id.into(), method, params))
    }

    /// Create a notification
    pub fn notification(method: impl Into<String>, params: Option<Value>) -> JsonRpcMessage {
        JsonRpcMessage::Notification(JsonRpcNotification::new(method, params))
    }

    /// Create a success response
    pub fn success_response(id: impl Into<Id>, result: Option<Value>) -> JsonRpcMessage {
        JsonRpcMessage::SuccessResponse(JsonRpcSuccessResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            result,
        })
    }

    /// Create an error response
    pub fn error_response(id: impl Into<Id>, error: JsonRpcError) -> JsonRpcMessage {
        JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id: id.into(),
            error,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = JsonRpcRequest::new(Id::Number(1), "session/new", None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""jsonrpc":"2.0""#));
        assert!(json.contains(r#""id":1"#));
        assert!(json.contains(r#""method":"session/new""#));
    }

    #[test]
    fn test_notification_serialization() {
        let notif = JsonRpcNotification::new("session/cancel", None);
        let json = serde_json::to_string(&notif).unwrap();
        assert!(json.contains(r#""jsonrpc":"2.0""#));
        assert!(json.contains(r#""method":"session/cancel""#));
        assert!(!json.contains(r#""id""#));
    }

    #[test]
    fn test_response_serialization() {
        let resp = JsonRpcResponse::success(Id::String("abc".to_string()), Some(json!("test")));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""jsonrpc":"2.0""#));
        assert!(json.contains(r#""id":"abc""#));
    }

    #[test]
    fn test_id_deserialization() {
        let id: Id = serde_json::from_str("42").unwrap();
        assert!(matches!(id, Id::Number(42)));

        let id: Id = serde_json::from_str(r#""hello""#).unwrap();
        assert!(matches!(id, Id::String(s) if s == "hello"));

        let id: Id = serde_json::from_str("null").unwrap();
        assert!(matches!(id, Id::Null));
    }

    #[test]
    fn test_roundtrip_request() {
        let req = JsonRpcRequest::new(Id::Number(1), "session/new", Some(json!({"cwd": "/test"})));
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "session/new");
        assert!(matches!(parsed.id, Id::Number(1)));
    }

    #[test]
    fn test_roundtrip_notification() {
        let notif = JsonRpcNotification::new("session/cancel", Some(json!({"session_id": "abc"})));
        let json = serde_json::to_string(&notif).unwrap();
        assert!(!json.contains(r#""id""#));
        let parsed: JsonRpcNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "session/cancel");
    }

    #[test]
    fn test_roundtrip_error_response() {
        let resp = JsonRpcResponse::error(
            Id::Number(5),
            JsonRpcError::new(JsonRpcError::METHOD_NOT_FOUND, "Method not found"),
        );
        let json = serde_json::to_string(&resp).unwrap();
        // Ensure it's an error response by checking the JSON structure
        assert!(json.contains(r#""error":{"#));
        // Deserialize as error response directly
        let parsed: JsonRpcErrorResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.error.code, JsonRpcError::METHOD_NOT_FOUND);
        assert_eq!(parsed.error.message, "Method not found");
    }

    #[test]
    fn test_message_variant_detection() {
        let req_json = r#"{"jsonrpc":"2.0","id":1,"method":"test"}"#;
        let msg: JsonRpcMessage = serde_json::from_str(req_json).unwrap();
        assert!(msg.is_request());
        assert!(msg.method() == Some("test"));
        assert!(msg.id().is_some());

        let notif_json = r#"{"jsonrpc":"2.0","method":"notify"}"#;
        let msg: JsonRpcMessage = serde_json::from_str(notif_json).unwrap();
        assert!(msg.is_notification());
        assert!(msg.id().is_none());

        let resp_json = r#"{"jsonrpc":"2.0","id":1,"result":"ok"}"#;
        let msg: JsonRpcMessage = serde_json::from_str(resp_json).unwrap();
        assert!(msg.is_response());
    }

    #[test]
    fn test_builder_pattern() {
        use super::builder;

        let req = builder::request(Id::Number(1), "init", None);
        assert!(req.is_request());

        let notif = builder::notification("ping", None);
        assert!(notif.is_notification());

        let resp = builder::success_response(Id::Number(1), Some(json!("done")));
        assert!(resp.is_response());

        let err = builder::error_response(
            Id::Number(1),
            JsonRpcError::new(JsonRpcError::INTERNAL_ERROR, "Oops"),
        );
        assert!(err.is_response());
    }
}
