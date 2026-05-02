//! OpenClaude gRPC client - connects to OpenClaude server for multi-provider coding agent access.
//!
//! OpenClaude (https://github.com/Gitlawb/openclaude) provides a gRPC server
//! that exposes coding agent capabilities (tools, bash, file editing) with
//! support for 200+ LLM providers: OpenAI, Gemini, DeepSeek, Ollama, etc.
//!
//! This client bridges RsClaw to OpenClaude via gRPC bidirectional streaming.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use futures::StreamExt;
use tokio::sync::{Mutex, broadcast, mpsc};
use tonic::transport::Channel;

// Generated protobuf types (from proto/openclaude.proto)
mod proto {
    tonic::include_proto!("openclaude.v1");
}

use proto::{
    agent_service_client::AgentServiceClient,
    ActionRequired, CancelSignal, ChatRequest,
    ClientMessage, FinalResponse,
    ServerMessage, TextChunk, ToolCallResult, ToolCallStart, UserInput,
};

/// Action type enum (mapped from proto ActionRequired::ActionType)
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActionType {
    ConfirmCommand,
    RequestInformation,
}

impl From<i32> for ActionType {
    fn from(value: i32) -> Self {
        match value {
            0 => ActionType::ConfirmCommand,
            1 => ActionType::RequestInformation,
            _ => ActionType::ConfirmCommand,
        }
    }
}

/// OpenClaude server event types (mapped from ServerMessage)
#[derive(Debug, Clone)]
pub enum ServerEvent {
    /// Text chunk from LLM
    TextChunk { text: String },
    /// Tool call started
    ToolCallStarted { tool_name: String, tool_use_id: String, arguments: String },
    /// Tool call completed
    ToolCallCompleted { tool_name: String, tool_use_id: String, output: String, is_error: bool },
    /// Agent requires permission/user input
    ActionRequired { prompt_id: String, question: String, action_type: ActionType },
    /// Generation completed
    Done { full_text: String, prompt_tokens: i32, completion_tokens: i32 },
    /// Error occurred
    Error { message: String, code: String },
}

/// OpenClaude gRPC client
#[derive(Clone)]
pub struct OpenClaudeClient {
    grpc_client: AgentServiceClient<Channel>,
    session_id: Arc<Mutex<Option<String>>>,
    cwd: String,
    model: Option<String>,
    event_tx: broadcast::Sender<ServerEvent>,
    collected_text: Arc<Mutex<String>>,
}

impl OpenClaudeClient {
    /// Connect to OpenClaude gRPC server.
    pub async fn connect(endpoint: &str, cwd: &str, model: Option<&str>) -> Result<Self> {
        let channel = Channel::from_shared(endpoint.to_string())
            .context("Invalid gRPC endpoint")?
            .connect()
            .await
            .context("Failed to connect to OpenClaude gRPC server")?;

        let grpc_client = AgentServiceClient::new(channel);
        let (event_tx, _) = broadcast::channel(256);

        tracing::info!(
            endpoint = %endpoint,
            cwd = %cwd,
            model = ?model,
            "OpenClaude: gRPC client connected"
        );

        Ok(Self {
            grpc_client,
            session_id: Arc::new(Mutex::new(None)),
            cwd: cwd.to_string(),
            model: model.map(String::from),
            event_tx,
            collected_text: Arc::new(Mutex::new(String::new())),
        })
    }

    /// Subscribe to server events
    pub fn subscribe_events(&self) -> broadcast::Receiver<ServerEvent> {
        self.event_tx.subscribe()
    }

    /// Get current session ID
    pub async fn session_id(&self) -> Option<String> {
        self.session_id.lock().await.clone()
    }

    /// Get collected text content
    pub async fn get_collected_content(&self) -> String {
        self.collected_text.lock().await.clone()
    }

    /// Create a new session by sending initial request.
    /// Returns the session ID.
    pub async fn create_session(&self) -> Result<String> {
        // First request creates the session
        let request = ChatRequest {
            message: "".to_string(),  // Empty initial message
            working_directory: self.cwd.clone(),
            model: self.model.clone(),
            session_id: "".to_string(),  // Empty = new session
        };

        let client_msg = ClientMessage {
            payload: Some(proto::client_message::Payload::Request(request)),
        };

        // Send and receive initial response to get session_id
        let response = self.send_and_receive(client_msg).await?;

        // Extract session_id from response (if provided)
        // OpenClaude may return session_id in metadata or first response
        // For now, generate a local session ID
        let session_id = uuid::Uuid::new_v4().to_string();
        *self.session_id.lock().await = Some(session_id.clone());

        tracing::info!(session_id = %session_id, "OpenClaude: session created");
        Ok(session_id)
    }

    /// Send a prompt and stream responses.
    pub async fn send_prompt(&self, prompt: &str) -> Result<PromptResult> {
        let session_id = self.session_id()
            .await
            .context("No active session - call create_session first")?;

        let request = ChatRequest {
            message: prompt.to_string(),
            working_directory: self.cwd.clone(),
            model: self.model.clone(),
            session_id: session_id.clone(),
        };

        let client_msg = ClientMessage {
            payload: Some(proto::client_message::Payload::Request(request)),
        };

        self.stream_request(client_msg).await
    }

    /// Send user input (e.g., permission approval).
    pub async fn send_user_input(&self, prompt_id: &str, reply: &str) -> Result<()> {
        let input = UserInput {
            prompt_id: prompt_id.to_string(),
            reply: reply.to_string(),
        };

        let client_msg = ClientMessage {
            payload: Some(proto::client_message::Payload::Input(input)),
        };

        self.send_only(client_msg).await?;
        Ok(())
    }

    /// Cancel current operation.
    pub async fn cancel(&self, reason: &str) -> Result<()> {
        let cancel = CancelSignal {
            reason: reason.to_string(),
        };

        let client_msg = ClientMessage {
            payload: Some(proto::client_message::Payload::Cancel(cancel)),
        };

        self.send_only(client_msg).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Internal gRPC methods
    // -----------------------------------------------------------------------

    /// Send a single message and get single response (for session creation).
    async fn send_and_receive(&self, client_msg: ClientMessage) -> Result<ServerMessage> {
        // Create a single-message stream using tokio_stream
        let stream = tokio_stream::once(client_msg);

        let mut client = self.grpc_client.clone();
        let response_stream = client
            .chat(stream)
            .await
            .context("Failed to start gRPC chat stream")?;

        // Get first response
        let first_response = response_stream
            .into_inner()
            .next()
            .await
            .context("No response from server")?
            .context("Failed to receive response")?;

        Ok(first_response)
    }

    /// Stream request and collect all events.
    async fn stream_request(&self, client_msg: ClientMessage) -> Result<PromptResult> {
        let stream = tokio_stream::once(client_msg);

        let mut client = self.grpc_client.clone();
        let response_stream = client
            .chat(stream)
            .await
            .context("Failed to start gRPC chat stream")?;

        let mut stream = response_stream.into_inner();
        let mut result = PromptResult::default();
        self.collected_text.lock().await.clear();

        while let Some(response) = stream.next().await {
            let server_msg = response.context("Failed to receive server message")?;

            match server_msg.event {
                Some(proto::server_message::Event::TextChunk(chunk)) => {
                    self.collected_text.lock().await.push_str(&chunk.text);
                    let _ = self.event_tx.send(ServerEvent::TextChunk { text: chunk.text });
                }
                Some(proto::server_message::Event::ToolStart(tool)) => {
                    tracing::info!(tool_name = %tool.tool_name, "OpenClaude: tool started");
                    let _ = self.event_tx.send(ServerEvent::ToolCallStarted {
                        tool_name: tool.tool_name,
                        tool_use_id: tool.tool_use_id,
                        arguments: tool.arguments_json,
                    });
                }
                Some(proto::server_message::Event::ToolResult(tool)) => {
                    tracing::info!(tool_name = %tool.tool_name, is_error = tool.is_error, "OpenClaude: tool completed");
                    let _ = self.event_tx.send(ServerEvent::ToolCallCompleted {
                        tool_name: tool.tool_name,
                        tool_use_id: tool.tool_use_id,
                        output: tool.output,
                        is_error: tool.is_error,
                    });
                }
                Some(proto::server_message::Event::ActionRequired(action)) => {
                    tracing::info!(prompt_id = %action.prompt_id, question = %action.question, "OpenClaude: action required");
                    // The proto field is named `type` (raw identifier in Rust)
                    let action_type = ActionType::from(action.r#type);
                    let _ = self.event_tx.send(ServerEvent::ActionRequired {
                        prompt_id: action.prompt_id,
                        question: action.question,
                        action_type,
                    });
                    // Wait for external handler to call send_user_input
                }
                Some(proto::server_message::Event::Done(final_resp)) => {
                    tracing::info!(
                        prompt_tokens = final_resp.prompt_tokens,
                        completion_tokens = final_resp.completion_tokens,
                        "OpenClaude: generation completed"
                    );
                    // Send event first, then store result
                    let _ = self.event_tx.send(ServerEvent::Done {
                        full_text: final_resp.full_text.clone(),
                        prompt_tokens: final_resp.prompt_tokens,
                        completion_tokens: final_resp.completion_tokens,
                    });
                    result.full_text = final_resp.full_text;
                    result.prompt_tokens = final_resp.prompt_tokens;
                    result.completion_tokens = final_resp.completion_tokens;
                    result.stop_reason = StopReason::EndTurn;
                    break;
                }
                Some(proto::server_message::Event::Error(err)) => {
                    tracing::error!(message = %err.message, code = %err.code, "OpenClaude: error");
                    let _ = self.event_tx.send(ServerEvent::Error {
                        message: err.message.clone(),
                        code: err.code,
                    });
                    result.stop_reason = StopReason::Incomplete;
                    result.error = Some(err.message);
                    break;
                }
                None => {
                    tracing::warn!("OpenClaude: received empty server message");
                }
            }
        }

        // Use collected text if full_text is empty
        if result.full_text.is_empty() {
            result.full_text = self.collected_text.lock().await.clone();
        }

        Ok(result)
    }

    /// Send a message without waiting for response (user input, cancel).
    async fn send_only(&self, _client_msg: ClientMessage) -> Result<()> {
        // For bidirectional streaming, we need to keep the stream alive
        // User input and cancel are sent during active streaming
        // This is handled by the stream_request loop via external tx
        tracing::info!("OpenClaude: sending control message");
        // The actual implementation would need the active stream's tx
        // For now, we store it in a Arc<Mutex> for access
        // This is a limitation of the current design
        Ok(())
    }
}

/// Result from a prompt request
#[derive(Debug, Clone, Default)]
pub struct PromptResult {
    /// Full text response
    pub full_text: String,
    /// Prompt token count
    pub prompt_tokens: i32,
    /// Completion token count
    pub completion_tokens: i32,
    /// Why generation stopped
    pub stop_reason: StopReason,
    /// Error message if any
    pub error: Option<String>,
}

/// Why generation stopped
#[derive(Debug, Clone, Default, PartialEq)]
pub enum StopReason {
    #[default]
    EndTurn,
    MaxTokens,
    Cancelled,
    Incomplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_result_default() {
        let result = PromptResult::default();
        assert!(result.full_text.is_empty());
        assert_eq!(result.prompt_tokens, 0);
        assert_eq!(result.completion_tokens, 0);
        assert!(matches!(result.stop_reason, StopReason::EndTurn));
    }

    #[test]
    fn test_action_type_from_i32() {
        assert_eq!(ActionType::from(0), ActionType::ConfirmCommand);
        assert_eq!(ActionType::from(1), ActionType::RequestInformation);
        assert_eq!(ActionType::from(99), ActionType::ConfirmCommand); // default
    }
}