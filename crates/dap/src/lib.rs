pub mod adapters;
pub mod client;
pub mod debugger_settings;
pub mod proto_conversions;
pub mod transport;

pub use dap_types::*;
pub use task::{DebugAdapterConfig, DebugAdapterKind, DebugRequestType};

pub type ScopeId = u64;
pub type VariableReference = u64;
pub type StackFrameId = u64;

#[cfg(any(test, feature = "test-support"))]
pub use adapters::FakeAdapter;
