//! parley core: wire protocol, daemon state, hook and MCP handlers. No audio here; the audio
//! pipeline lives in parleyd so the hook binary stays small and starts fast.

pub mod client;
pub mod daemon;
pub mod format;
pub mod hook;
pub mod mcp;
pub mod proto;
pub mod voice;
