//! Tool execution bridge: a `ToolHost` delegating to the hermes-tools
//! registry. Tool failures are surfaced to the model as `{"error": ...}` JSON
//! by the registry (`registry.dispatch` contract), never as panics.

use std::future::Future;
use std::sync::Arc;

use hermes_core::agent::ToolHost;
use hermes_tools::ToolRegistry;

/// Runs tool calls on behalf of the streaming turn loop.
#[derive(Clone)]
pub struct RegistryHost {
    registry: Arc<ToolRegistry>,
}

impl Default for RegistryHost {
    fn default() -> Self {
        Self {
            registry: Arc::new(ToolRegistry::new()),
        }
    }
}

impl RegistryHost {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self { registry }
    }
}

impl ToolHost for RegistryHost {
    fn execute(
        &self,
        name: &str,
        arguments: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send {
        let registry = Arc::clone(&self.registry);
        let name = name.to_string();
        let arguments = arguments.to_string();
        Box::pin(async move {
            let args: serde_json::Value = serde_json::from_str(&arguments)
                .map_err(|e| anyhow::anyhow!("invalid tool arguments JSON: {e}"))?;
            Ok(registry.dispatch(&name, &args).await)
        })
    }
}
