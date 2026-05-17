//! google.rpc-style structured error details for A2A JSON-RPC.

use serde_json::json;

use super::types::JsonRpcError;

pub fn invalid_argument(msg: impl Into<String>, field: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: -32602,
        message: msg.into(),
        data: Some(json!({
            "@type": "type.googleapis.com/google.rpc.BadRequest",
            "fieldViolations": [{ "field": field.into() }]
        })),
    }
}

pub fn not_found(resource: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: -32001,
        message: "resource not found".to_owned(),
        data: Some(json!({
            "@type": "type.googleapis.com/google.rpc.ResourceInfo",
            "resourceName": resource.into(),
        })),
    }
}

pub fn precondition_failed(reason: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: -32002,
        message: "precondition failed".to_owned(),
        data: Some(json!({
            "@type": "type.googleapis.com/google.rpc.PreconditionFailure",
            "violations": [{ "type": reason.into() }],
        })),
    }
}

pub fn internal(msg: impl Into<String>) -> JsonRpcError {
    JsonRpcError {
        code: -32603,
        message: msg.into(),
        data: None,
    }
}
