//! hermes-core: agent loop, transports, and message model.
//! Port of `agent/` from the Python hermes-agent (contract: message/tool-call
//! JSON shapes and provider wire formats stay identical).

pub mod model;
pub mod test_transport;
pub mod transport;
pub mod agent;
pub mod config;
pub mod transports;

pub use model::{Message, Role, ToolCall};
