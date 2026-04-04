//! ACP (Agent Client Protocol) implementation
//!
//! Port of @agentclientprotocol/sdk from TypeScript to Rust
//! Protocol spec: https://agentclientprotocol.com

pub mod client;
pub mod gateway;
pub mod gateway_client;
pub mod jsonrpc;
pub mod notification;
pub mod opencode_client;
pub mod stream;
pub mod types;

pub use client::{AcpCallbackHandler, AcpClient, DefaultAcpHandler, SessionEvent};
pub use gateway::client::GatewayClient;
pub use jsonrpc::{
    Id, JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcResponse, JsonRpcSuccessResponse, builder,
};
pub use notification::{Notification, NotificationManager, NotificationPriority, NotificationSink};
pub use opencode_client::OpenCodeClient;
pub use stream::{
    NdJsonCodec, NdJsonStream, ProcessReader, ProcessWriter, StdinReader, StdioWriter,
    SubprocessStream,
};
pub use types::*;
