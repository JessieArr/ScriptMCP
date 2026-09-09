use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
    Implementation, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer};
use serde_json::Value;
use tracing::error;

use crate::catalog::Catalog;
use crate::config::Opts;
use crate::deno::DenoRuntime;

#[derive(Clone)]
pub struct ScriptMcp {
    catalog: Arc<Catalog>,
    runtime: Arc<DenoRuntime>,
    scripts_dir: String,
}

impl ScriptMcp {
    pub fn new(opts: &Opts, catalog: Catalog, runtime: DenoRuntime) -> Self {
        Self {
            catalog: Arc::new(catalog),
            runtime: Arc::new(runtime),
            scripts_dir: opts.scripts.display().to_string(),
        }
    }
}

impl ServerHandler for ScriptMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("scriptmcp", env!("CARGO_PKG_VERSION"))
                    .with_title("ScriptMCP")
                    .with_description("Expose Deno scripts as MCP tools"),
            )
            .with_instructions(format!(
                "Each JavaScript/TypeScript file in {} is an MCP tool. Call a tool to run that script in Deno. Arguments are passed as JSON matching the tool's input schema.",
                self.scripts_dir
            ))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.catalog.get(name).map(script_to_tool)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.catalog.tools().iter().map(script_to_tool).collect(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(script) = self.catalog.get(&request.name) else {
            return Err(ErrorData::invalid_params(
                format!("unknown tool: {}", request.name),
                None,
            ));
        };

        let arguments = match request.arguments {
            Some(map) => Value::Object(map),
            None => Value::Object(Default::default()),
        };

        match self
            .runtime
            .invoke(&script.path, &script.meta.permissions, &arguments)
            .await
        {
            Ok(value) => Ok(value_to_result(value).into()),
            Err(error) => {
                error!(tool = %request.name, error = %error, "script tool failed");
                Ok(CallToolResult::error(vec![ContentBlock::text(error.to_string())]).into())
            }
        }
    }
}

fn script_to_tool(script: &crate::catalog::ScriptTool) -> Tool {
    Tool::new(
        script.meta.name.clone(),
        script.meta.description.clone(),
        Catalog::input_schema_object(&script.meta.input_schema),
    )
}

fn value_to_result(value: Value) -> CallToolResult {
    match value {
        Value::Null => CallToolResult::success(vec![ContentBlock::text("null")]),
        Value::String(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        other => CallToolResult::structured(other),
    }
}
