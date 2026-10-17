//! AI auto-tagging (docs/plans/ai-auto-tagging/plan.md): settings and credentials, and runs
//! that tag assets in the background through a `TagProvider`.

pub mod anthropic;
pub mod claude_cli;
pub mod credentials;
pub mod engine;
#[cfg(debug_assertions)]
pub mod fake;
pub mod http;
pub mod jev;
pub mod pricing;
pub mod provider;
pub mod review;
pub mod settings;
pub mod store;
pub mod vocabulary;
