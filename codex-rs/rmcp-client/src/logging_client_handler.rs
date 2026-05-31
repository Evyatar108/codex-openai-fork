use std::sync::Arc;

use codex_mcp_notification_bridge::NotificationBridge;
use rmcp::ClientHandler;
use rmcp::RoleClient;
use rmcp::model::CancelledNotificationParam;
use rmcp::model::ClientInfo;
use rmcp::model::CreateElicitationRequestParams;
use rmcp::model::CreateElicitationResult;
use rmcp::model::LoggingLevel;
use rmcp::model::LoggingMessageNotificationParam;
use rmcp::model::ProgressNotificationParam;
use rmcp::model::ResourceUpdatedNotificationParam;
use rmcp::service::NotificationContext;
use rmcp::service::RequestContext;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::rmcp_client::SendElicitation;

#[derive(Clone)]
pub(crate) struct LoggingClientHandler {
    client_info: ClientInfo,
    send_elicitation: Arc<SendElicitation>,
    // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
    bridge: Option<Arc<NotificationBridge>>,
}

impl LoggingClientHandler {
    pub(crate) fn new(
        client_info: ClientInfo,
        send_elicitation: SendElicitation,
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        bridge: Option<Arc<NotificationBridge>>,
    ) -> Self {
        Self {
            client_info,
            send_elicitation: Arc::new(send_elicitation),
            bridge,
        }
    }
}

impl ClientHandler for LoggingClientHandler {
    async fn create_elicitation(
        &self,
        request: CreateElicitationRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<CreateElicitationResult, rmcp::ErrorData> {
        (self.send_elicitation)(context.id, request)
            .await
            .map(Into::into)
            .map_err(|err| rmcp::ErrorData::internal_error(err.to_string(), None))
    }

    async fn on_cancelled(
        &self,
        params: CancelledNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!(
            "MCP server cancelled request (request_id: {}, reason: {:?})",
            params.request_id, params.reason
        );
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge.forward_cancelled(params).await;
        }
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!(
            "MCP server progress notification (token: {:?}, progress: {}, total: {:?}, message: {:?})",
            params.progress_token, params.progress, params.total, params.message
        );
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge.forward_progress(params).await;
        }
    }

    async fn on_resource_updated(
        &self,
        params: ResourceUpdatedNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        info!("MCP server resource updated (uri: {})", params.uri);
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge.forward_resource_updated(params).await;
        }
    }

    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server resource list changed");
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge.forward_resource_list_changed().await;
        }
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server tool list changed");
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge.forward_tool_list_changed().await;
        }
    }

    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        info!("MCP server prompt list changed");
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge.forward_prompt_list_changed().await;
        }
    }

    fn get_info(&self) -> ClientInfo {
        self.client_info.clone()
    }

    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let LoggingMessageNotificationParam {
            level,
            logger,
            data,
        } = params;
        let logger_ref = logger.as_deref();
        match level {
            LoggingLevel::Emergency
            | LoggingLevel::Alert
            | LoggingLevel::Critical
            | LoggingLevel::Error => {
                error!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger_ref, data
                );
            }
            LoggingLevel::Warning => {
                warn!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger_ref, data
                );
            }
            LoggingLevel::Notice | LoggingLevel::Info => {
                info!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger_ref, data
                );
            }
            LoggingLevel::Debug => {
                debug!(
                    "MCP server log message (level: {:?}, logger: {:?}, data: {})",
                    level, logger_ref, data
                );
            }
        }
        // SANDBOX PATCH: invariant 25 (mcp-server-notifications)
        if let Some(bridge) = &self.bridge {
            bridge
                .forward_logging_message(LoggingMessageNotificationParam {
                    level,
                    logger,
                    data,
                })
                .await;
        }
    }
}
