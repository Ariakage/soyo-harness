//! MCP resources, resource templates, prompts, subscriptions, and safe elicitation (`F-02`).

use super::{
    McpClient, McpContent, McpLimits, NotificationTask, bounded, error, projection, service_error,
};
use crate::boundary::{Cancellation, Operation};
use crate::contract::BoundaryError;
use rmcp::model::{
    ElicitRequestParams, ElicitResult, ElicitationAction, GetPromptRequestParams, JsonObject,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ResourceContents, Role,
    ServerNotification, SubscribeRequestParams, SubscriptionFilterBuilder,
    UnsubscribeRequestParams,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::future::{Future, pending};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

const MAX_URI_BYTES: usize = 2048;
const MAX_PROMPT_NAME_BYTES: usize = 128;
const MAX_PROMPT_ARGS: usize = 64;
const MAX_PROMPT_ARG_BYTES: usize = 16 * 1024;
const MAX_UPDATED_URIS: usize = 256;
const MAX_ELICIT_MESSAGE_BYTES: usize = 4096;
const MAX_ELICIT_CONTENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerCapabilities {
    pub tools: bool,
    pub tools_list_changed: bool,
    pub resources: bool,
    pub resources_subscribe: bool,
    pub resources_list_changed: bool,
    pub prompts: bool,
    pub prompts_list_changed: bool,
    /// SEP-2663 `io.modelcontextprotocol/tasks` extension advertisement.
    pub tasks: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceTemplate {
    pub uri_template: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceCatalog {
    pub resources: Vec<McpResource>,
    pub templates: Vec<McpResourceTemplate>,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpResourceContent {
    Text {
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        text: String,
    },
    Blob {
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        encoded_bytes: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResourceReadResult {
    pub contents: Vec<McpResourceContent>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPrompt {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpPromptCatalog {
    pub prompts: Vec<McpPrompt>,
    pub generation: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: Vec<McpContent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub messages: Vec<McpPromptMessage>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpElicitationRequest {
    Form {
        server_id: String,
        message: String,
        requested_schema: Value,
    },
    Url {
        server_id: String,
        message: String,
        url: String,
        elicitation_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum McpElicitationDecision {
    Accept {
        #[serde(default)]
        content: Option<Value>,
    },
    Decline,
    Cancel,
}

pub trait McpElicitationHandler: Send + Sync {
    fn elicit<'a>(
        &'a self,
        request: &'a McpElicitationRequest,
        cancel: &'a dyn Cancellation,
    ) -> Operation<'a, McpElicitationDecision>;
}

struct NeverCancel;
impl Cancellation for NeverCancel {
    fn is_cancelled(&self) -> bool {
        false
    }
    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(pending())
    }
}

fn check_bounded_text(value: &str, max_bytes: usize, label: &str) -> Result<String, BoundaryError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(crate::text::is_terminal_control)
    {
        return Err(error(
            "mcp_protocol",
            &format!("MCP {label} is empty, oversized, or contains control characters"),
        ));
    }
    Ok(value.to_owned())
}

fn check_optional_text(value: Option<String>, max_bytes: usize) -> Option<String> {
    value.and_then(|text| {
        if text.is_empty() || text.chars().any(crate::text::is_terminal_control) {
            return None;
        }
        Some(truncate_owned(text, max_bytes))
    })
}

fn truncate_owned(mut text: String, mut limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    while !text.is_char_boundary(limit) {
        limit -= 1;
    }
    text.truncate(limit);
    text
}

pub(super) fn push_updated_uri(target: &std::sync::Mutex<Vec<String>>, uri: &str) {
    if uri.is_empty()
        || uri.len() > MAX_URI_BYTES
        || uri.chars().any(crate::text::is_terminal_control)
    {
        return;
    }
    if let Ok(mut guard) = target.lock() {
        if guard.iter().any(|existing| existing == uri) {
            return;
        }
        if guard.len() >= MAX_UPDATED_URIS {
            guard.remove(0);
        }
        guard.push(uri.to_owned());
    }
}

pub(super) async fn handle_elicitation(
    server_id: &str,
    handler: Option<&Arc<dyn McpElicitationHandler>>,
    params: ElicitRequestParams,
) -> ElicitResult {
    let Some(handler) = handler else {
        return ElicitResult::new(ElicitationAction::Decline);
    };
    let request = match params {
        ElicitRequestParams::FormElicitationParams {
            message,
            requested_schema,
            ..
        } => {
            if check_bounded_text(&message, MAX_ELICIT_MESSAGE_BYTES, "elicitation message")
                .is_err()
            {
                return ElicitResult::new(ElicitationAction::Decline);
            }
            let Ok(schema_value) = serde_json::to_value(requested_schema) else {
                return ElicitResult::new(ElicitationAction::Decline);
            };
            let Ok(encoded) = serde_json::to_vec(&schema_value) else {
                return ElicitResult::new(ElicitationAction::Decline);
            };
            if encoded.len() > MAX_ELICIT_CONTENT_BYTES {
                return ElicitResult::new(ElicitationAction::Decline);
            }
            McpElicitationRequest::Form {
                server_id: server_id.to_owned(),
                message,
                requested_schema: schema_value,
            }
        }
        ElicitRequestParams::UrlElicitationParams {
            message,
            url,
            elicitation_id,
            ..
        } => {
            if check_bounded_text(&message, MAX_ELICIT_MESSAGE_BYTES, "elicitation message")
                .is_err()
                || check_bounded_text(&url, MAX_URI_BYTES, "elicitation url").is_err()
                || elicitation_id.len() > 256
                || elicitation_id.chars().any(crate::text::is_terminal_control)
            {
                return ElicitResult::new(ElicitationAction::Decline);
            }
            let Ok(parsed) = reqwest::Url::parse(&url) else {
                return ElicitResult::new(ElicitationAction::Decline);
            };
            if parsed.scheme() != "https"
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.host_str().is_none()
            {
                return ElicitResult::new(ElicitationAction::Decline);
            }
            McpElicitationRequest::Url {
                server_id: server_id.to_owned(),
                message,
                url,
                elicitation_id,
            }
        }
        _ => return ElicitResult::new(ElicitationAction::Decline),
    };
    let cancel = NeverCancel;
    match tokio::time::timeout(Duration::from_secs(30), handler.elicit(&request, &cancel)).await {
        Ok(Ok(McpElicitationDecision::Accept { content })) => {
            let mut result = ElicitResult::new(ElicitationAction::Accept);
            if let Some(value) = content {
                let Ok(bytes) = serde_json::to_vec(&value) else {
                    return ElicitResult::new(ElicitationAction::Decline);
                };
                if bytes.len() > MAX_ELICIT_CONTENT_BYTES {
                    return ElicitResult::new(ElicitationAction::Decline);
                }
                result = result.with_content(value);
            }
            result
        }
        Ok(Ok(McpElicitationDecision::Cancel)) => ElicitResult::new(ElicitationAction::Cancel),
        Ok(Ok(McpElicitationDecision::Decline)) | Ok(Err(_)) | Err(_) => {
            ElicitResult::new(ElicitationAction::Decline)
        }
    }
}

async fn collect_paginated<Page, Item, F, Fut>(
    cancel: &dyn Cancellation,
    limits: &McpLimits,
    mut fetch_page: F,
    mut next_cursor: impl FnMut(&Page) -> Option<String>,
    mut extract: impl FnMut(Page) -> Vec<Item>,
) -> Result<Vec<Item>, BoundaryError>
where
    Item: Serialize,
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Page, rmcp::ServiceError>>,
{
    let mut items = Vec::new();
    let mut cursors = BTreeSet::new();
    let mut cursor = None;
    let mut total_bytes = 0usize;
    for _ in 0..limits.max_pages {
        if cancel.is_cancelled() {
            return Err(error("cancelled", "MCP request cancelled"));
        }
        let page = fetch_page(cursor).await.map_err(service_error)?;
        let next = next_cursor(&page);
        for item in extract(page) {
            let size = serde_json::to_vec(&item)
                .map_err(|_| error("mcp_protocol", "MCP catalog entry encoding failed"))?
                .len();
            total_bytes = total_bytes.saturating_add(size);
            if items.len() >= limits.max_tools || total_bytes > limits.max_catalog_bytes {
                return Err(error(
                    "mcp_limit",
                    "MCP resource or prompt catalog exceeds configured limits",
                ));
            }
            items.push(item);
        }
        let Some(next) = next else {
            return Ok(items);
        };
        if !cursors.insert(next.clone()) {
            return Err(error("mcp_protocol", "MCP pagination cursor repeated"));
        }
        cursor = Some(next);
    }
    Err(error(
        "mcp_limit",
        "MCP catalogue pagination exceeds configured limit",
    ))
}

impl McpClient {
    /// Capabilities advertised by the connected MCP server during initialization.
    pub fn server_capabilities(&self) -> McpServerCapabilities {
        let Some(info) = self.service.peer_info() else {
            return McpServerCapabilities::default();
        };
        let caps = &info.capabilities;
        McpServerCapabilities {
            tools: caps.tools.is_some(),
            tools_list_changed: caps
                .tools
                .as_ref()
                .is_some_and(|t| t.list_changed == Some(true)),
            resources: caps.resources.is_some(),
            resources_subscribe: caps
                .resources
                .as_ref()
                .is_some_and(|r| r.subscribe == Some(true)),
            resources_list_changed: caps
                .resources
                .as_ref()
                .is_some_and(|r| r.list_changed == Some(true)),
            prompts: caps.prompts.is_some(),
            prompts_list_changed: caps
                .prompts
                .as_ref()
                .is_some_and(|p| p.list_changed == Some(true)),
            tasks: caps.supports_tasks(),
        }
    }

    pub fn resource_generation(&self) -> u64 {
        self.resource_generation.load(Ordering::Acquire)
    }

    pub fn prompt_generation(&self) -> u64 {
        self.prompt_generation.load(Ordering::Acquire)
    }

    /// Drain recently notified updated resource URIs observed from server notifications.
    pub fn take_updated_resources(&self) -> Vec<String> {
        self.updated_resources
            .lock()
            .map(|mut guard| std::mem::take(&mut *guard))
            .unwrap_or_default()
    }

    /// Snapshot pending updated resource URIs without clearing them.
    pub fn peek_updated_resources(&self) -> Vec<String> {
        self.updated_resources
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    pub async fn list_resources(
        &self,
        cancel: &dyn Cancellation,
    ) -> Result<McpResourceCatalog, BoundaryError> {
        bounded(
            self.list_resources_inner(cancel),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn list_resources_inner(
        &self,
        cancel: &dyn Cancellation,
    ) -> Result<McpResourceCatalog, BoundaryError> {
        self.ensure_connected()?;
        let caps = self.server_capabilities();
        if !caps.resources {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise resources capability",
            ));
        }
        let generation = self.resource_generation();
        let raw = collect_paginated(
            cancel,
            &self.limits,
            |cursor| {
                let mut params = PaginatedRequestParams::default();
                params.cursor = cursor;
                self.service.list_resources(Some(params))
            },
            |page| page.next_cursor.clone(),
            |page| page.resources,
        )
        .await?;
        let mut seen = BTreeSet::new();
        let mut resources = Vec::with_capacity(raw.len());
        for entry in raw {
            let uri = check_bounded_text(&entry.uri, MAX_URI_BYTES, "resource URI")?;
            if !seen.insert(uri.clone()) {
                return Err(error(
                    "mcp_protocol",
                    "MCP catalog has duplicate resource URIs",
                ));
            }
            let name = check_bounded_text(&entry.name, 256, "resource name")?;
            resources.push(McpResource {
                uri,
                name,
                title: check_optional_text(entry.title.clone(), 256),
                description: check_optional_text(entry.description.clone(), 2048),
                mime_type: check_optional_text(entry.mime_type.clone(), 256),
                size: entry.size,
            });
        }
        resources.sort_by(|a, b| a.uri.cmp(&b.uri));
        if generation != self.resource_generation() {
            return Err(error(
                "mcp_stale_catalog",
                "MCP resource catalog changed during discovery",
            ));
        }
        Ok(McpResourceCatalog {
            resources,
            templates: Vec::new(),
            generation,
        })
    }

    pub async fn list_resource_templates(
        &self,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<McpResourceTemplate>, BoundaryError> {
        bounded(
            self.list_resource_templates_inner(cancel),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn list_resource_templates_inner(
        &self,
        cancel: &dyn Cancellation,
    ) -> Result<Vec<McpResourceTemplate>, BoundaryError> {
        self.ensure_connected()?;
        if !self.server_capabilities().resources {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise resources capability",
            ));
        }
        let raw = collect_paginated(
            cancel,
            &self.limits,
            |cursor| {
                let mut params = PaginatedRequestParams::default();
                params.cursor = cursor;
                self.service.list_resource_templates(Some(params))
            },
            |page| page.next_cursor.clone(),
            |page| page.resource_templates,
        )
        .await?;
        let mut seen = BTreeSet::new();
        let mut templates = Vec::with_capacity(raw.len());
        for entry in raw {
            let uri_template =
                check_bounded_text(&entry.uri_template, MAX_URI_BYTES, "resource uriTemplate")?;
            if !seen.insert(uri_template.clone()) {
                return Err(error(
                    "mcp_protocol",
                    "MCP catalog has duplicate resource templates",
                ));
            }
            let name = check_bounded_text(&entry.name, 256, "resource template name")?;
            templates.push(McpResourceTemplate {
                uri_template,
                name,
                title: check_optional_text(entry.title.clone(), 256),
                description: check_optional_text(entry.description.clone(), 2048),
                mime_type: check_optional_text(entry.mime_type.clone(), 256),
            });
        }
        templates.sort_by(|a, b| a.uri_template.cmp(&b.uri_template));
        Ok(templates)
    }

    pub async fn read_resource(
        &self,
        uri: &str,
        cancel: &dyn Cancellation,
    ) -> Result<McpResourceReadResult, BoundaryError> {
        let uri = check_bounded_text(uri, MAX_URI_BYTES, "resource URI")?;
        bounded(
            self.read_resource_inner(uri),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn read_resource_inner(
        &self,
        uri: String,
    ) -> Result<McpResourceReadResult, BoundaryError> {
        self.ensure_connected()?;
        if !self.server_capabilities().resources {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise resources capability",
            ));
        }
        let result = self
            .service
            .read_resource(ReadResourceRequestParams::new(uri))
            .await
            .map_err(service_error)?;
        let wire_size = serde_json::to_vec(&result)
            .map_err(|_| error("mcp_protocol", "MCP resource reply cannot be encoded"))?
            .len();
        if wire_size > self.limits.max_wire_bytes {
            return Err(error(
                "mcp_limit",
                "MCP resource output exceeds the wire-message limit",
            ));
        }
        let mut truncated = result.contents.len() > self.limits.max_content_blocks;
        let mut remaining = self.limits.max_result_bytes.saturating_sub(128);
        let mut contents = Vec::new();
        for block in result
            .contents
            .into_iter()
            .take(self.limits.max_content_blocks)
        {
            let item = match block {
                ResourceContents::TextResourceContents {
                    uri,
                    mime_type,
                    text,
                    ..
                } => {
                    let uri = truncate_owned(
                        check_bounded_text(&uri, MAX_URI_BYTES, "resource URI")?,
                        MAX_URI_BYTES,
                    );
                    let mime_type = check_optional_text(mime_type, 256);
                    let mut item = McpResourceContent::Text {
                        uri,
                        mime_type,
                        text,
                    };
                    loop {
                        let size = serde_json::to_vec(&item)
                            .map_err(|_| {
                                error("mcp_protocol", "MCP resource content cannot be encoded")
                            })?
                            .len()
                            .saturating_add(1);
                        if size <= remaining {
                            remaining -= size;
                            break Some(item);
                        }
                        truncated = true;
                        match &mut item {
                            McpResourceContent::Text { text, .. } if !text.is_empty() => {
                                *text = truncate_owned(std::mem::take(text), text.len() / 2);
                            }
                            _ => {
                                remaining = 0;
                                break None;
                            }
                        }
                    }
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => {
                    let uri = truncate_owned(
                        check_bounded_text(&uri, MAX_URI_BYTES, "resource URI")?,
                        MAX_URI_BYTES,
                    );
                    let item = McpResourceContent::Blob {
                        uri,
                        mime_type: check_optional_text(mime_type, 256),
                        encoded_bytes: blob.len(),
                    };
                    let size = serde_json::to_vec(&item)
                        .map_err(|_| error("mcp_protocol", "MCP resource blob cannot be encoded"))?
                        .len()
                        .saturating_add(1);
                    if size <= remaining {
                        remaining -= size;
                        Some(item)
                    } else {
                        truncated = true;
                        remaining = 0;
                        None
                    }
                }
                _ => None,
            };
            if let Some(item) = item {
                contents.push(item);
            }
            if remaining == 0 {
                break;
            }
        }
        Ok(McpResourceReadResult {
            contents,
            truncated,
        })
    }

    pub async fn subscribe_resource(
        &self,
        uri: &str,
        cancel: &dyn Cancellation,
    ) -> Result<(), BoundaryError> {
        let uri = check_bounded_text(uri, MAX_URI_BYTES, "resource URI")?;
        bounded(
            self.subscribe_resource_inner(uri),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn subscribe_resource_inner(&self, uri: String) -> Result<(), BoundaryError> {
        self.ensure_connected()?;
        if !self.server_capabilities().resources_subscribe {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not support resource subscriptions",
            ));
        }
        let mut guard = self.subscribed_uris.lock().await;
        if guard.contains(&uri) {
            return Ok(());
        }
        if guard.len() >= self.limits.max_tools {
            return Err(error("mcp_limit", "MCP subscribed resource limit reached"));
        }
        let version = self
            .service
            .peer_info()
            .map(|info| info.protocol_version.clone());
        if version == Some(ProtocolVersion::V_2026_07_28) {
            let mut stream = self
                .service
                .listen(
                    SubscriptionFilterBuilder::default()
                        .resource_subscription(uri.clone())
                        .build(),
                )
                .await
                .map_err(service_error)?;
            let updated_resources = self.updated_resources.clone();
            let resource_generation = self.resource_generation.clone();
            let task = NotificationTask(tokio::spawn(async move {
                while let Ok(Some(notification)) = stream.next().await {
                    if let ServerNotification::ResourceUpdatedNotification(note) = notification {
                        push_updated_uri(&updated_resources, &note.params.uri);
                        resource_generation.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }));
            if let Ok(mut tasks) = self.subscription_tasks.lock() {
                tasks.push(task);
            }
        } else {
            #[allow(deprecated)]
            self.service
                .subscribe(SubscribeRequestParams::new(uri.clone()))
                .await
                .map_err(service_error)?;
        }
        guard.insert(uri);
        Ok(())
    }

    pub async fn unsubscribe_resource(
        &self,
        uri: &str,
        cancel: &dyn Cancellation,
    ) -> Result<(), BoundaryError> {
        let uri = check_bounded_text(uri, MAX_URI_BYTES, "resource URI")?;
        bounded(
            self.unsubscribe_resource_inner(uri),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn unsubscribe_resource_inner(&self, uri: String) -> Result<(), BoundaryError> {
        self.ensure_connected()?;
        if !self.server_capabilities().resources_subscribe {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not support resource subscriptions",
            ));
        }
        let mut guard = self.subscribed_uris.lock().await;
        if !guard.contains(&uri) {
            return Ok(());
        }
        let version = self
            .service
            .peer_info()
            .map(|info| info.protocol_version.clone());
        if version != Some(ProtocolVersion::V_2026_07_28) {
            #[allow(deprecated)]
            self.service
                .unsubscribe(UnsubscribeRequestParams::new(uri.clone()))
                .await
                .map_err(service_error)?;
        }
        guard.remove(&uri);
        Ok(())
    }

    pub async fn list_prompts(
        &self,
        cancel: &dyn Cancellation,
    ) -> Result<McpPromptCatalog, BoundaryError> {
        bounded(
            self.list_prompts_inner(cancel),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn list_prompts_inner(
        &self,
        cancel: &dyn Cancellation,
    ) -> Result<McpPromptCatalog, BoundaryError> {
        self.ensure_connected()?;
        if !self.server_capabilities().prompts {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise prompts capability",
            ));
        }
        let generation = self.prompt_generation();
        let raw = collect_paginated(
            cancel,
            &self.limits,
            |cursor| {
                let mut params = PaginatedRequestParams::default();
                params.cursor = cursor;
                self.service.list_prompts(Some(params))
            },
            |page| page.next_cursor.clone(),
            |page| page.prompts,
        )
        .await?;
        let mut seen = BTreeSet::new();
        let mut prompts = Vec::with_capacity(raw.len());
        for entry in raw {
            let name = check_bounded_text(&entry.name, MAX_PROMPT_NAME_BYTES, "prompt name")?;
            if !seen.insert(name.clone()) {
                return Err(error(
                    "mcp_protocol",
                    "MCP catalog has duplicate prompt names",
                ));
            }
            let mut arguments = Vec::new();
            if let Some(args) = entry.arguments {
                for arg in args.into_iter().take(MAX_PROMPT_ARGS) {
                    let arg_name =
                        check_bounded_text(&arg.name, MAX_PROMPT_NAME_BYTES, "prompt arg")?;
                    arguments.push(McpPromptArgument {
                        name: arg_name,
                        title: check_optional_text(arg.title, 256),
                        description: check_optional_text(arg.description, 1024),
                        required: arg.required.unwrap_or(false),
                    });
                }
            }
            prompts.push(McpPrompt {
                name,
                title: check_optional_text(entry.title, 256),
                description: check_optional_text(entry.description, 2048),
                arguments,
            });
        }
        prompts.sort_by(|a, b| a.name.cmp(&b.name));
        if generation != self.prompt_generation() {
            return Err(error(
                "mcp_stale_catalog",
                "MCP prompt catalog changed during discovery",
            ));
        }
        Ok(McpPromptCatalog {
            prompts,
            generation,
        })
    }

    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: Option<BTreeMap<String, String>>,
        cancel: &dyn Cancellation,
    ) -> Result<McpPromptResult, BoundaryError> {
        let name = check_bounded_text(name, MAX_PROMPT_NAME_BYTES, "prompt name")?;
        let mut args_map = JsonObject::new();
        if let Some(arguments) = arguments {
            if arguments.len() > MAX_PROMPT_ARGS {
                return Err(error(
                    "invalid_args",
                    "MCP prompt argument count exceeds limit",
                ));
            }
            for (k, v) in arguments {
                check_bounded_text(&k, MAX_PROMPT_NAME_BYTES, "prompt argument name")?;
                if v.len() > MAX_PROMPT_ARG_BYTES || v.chars().any(crate::text::is_terminal_control)
                {
                    return Err(error(
                        "invalid_args",
                        "MCP prompt argument value is oversized or invalid",
                    ));
                }
                args_map.insert(k, Value::String(v));
            }
        }
        bounded(
            self.get_prompt_inner(name, args_map),
            self.limits.request_timeout,
            cancel,
        )
        .await?
    }

    async fn get_prompt_inner(
        &self,
        name: String,
        arguments: JsonObject,
    ) -> Result<McpPromptResult, BoundaryError> {
        self.ensure_connected()?;
        if !self.server_capabilities().prompts {
            return Err(error(
                "mcp_unsupported",
                "MCP server does not advertise prompts capability",
            ));
        }
        let mut params = GetPromptRequestParams::new(name);
        if !arguments.is_empty() {
            params = params.with_arguments(arguments);
        }
        let result = self
            .service
            .get_prompt(params)
            .await
            .map_err(service_error)?;
        let wire_size = serde_json::to_vec(&result)
            .map_err(|_| error("mcp_protocol", "MCP prompt reply cannot be encoded"))?
            .len();
        if wire_size > self.limits.max_wire_bytes {
            return Err(error(
                "mcp_limit",
                "MCP prompt output exceeds the wire-message limit",
            ));
        }
        let mut truncated = result.messages.len() > self.limits.max_content_blocks;
        let mut remaining = self.limits.max_result_bytes.saturating_sub(128);
        let mut messages = Vec::new();
        for msg in result
            .messages
            .into_iter()
            .take(self.limits.max_content_blocks)
        {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            }
            .to_owned();
            let content = projection::project_content_blocks(
                vec![msg.content],
                1,
                &mut remaining,
                &mut truncated,
            )?;
            if !content.is_empty() {
                messages.push(McpPromptMessage { role, content });
            }
            if remaining == 0 {
                break;
            }
        }
        Ok(McpPromptResult {
            description: check_optional_text(result.description, 2048),
            messages,
            truncated,
        })
    }
}
