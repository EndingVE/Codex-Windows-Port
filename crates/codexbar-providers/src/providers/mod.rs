//! One module per provider — the whole job for a provider worker.
//!
//! Add `providers/<name>.rs`, implement [`codexbar_core::Provider`], then wire it
//! into [`crate::live_registry`]. All 14 shipping providers are already wired;
//! nothing in `codexbar-core` changes, and no other provider's file is touched.
//!
//! [`openrouter`] is the reference: it shows the shape every module should have
//! (constructor pair, no-panic `fetch`, fixture tests, HTTPS endpoint policy,
//! credential precedence, soft degradation for optional endpoints).

pub mod claude;
pub mod codex;
pub mod copilot;
pub mod cursor;
pub mod deepseek;
pub mod elevenlabs;
pub mod gemini;
pub mod groq;
pub mod kimi;
pub mod minimax;
pub mod opencodego;
pub mod openrouter;
pub mod pending;
pub mod xai;
pub mod zai;

pub use claude::ClaudeOAuth;
pub use codex::Codex;
pub use copilot::Copilot;
pub use cursor::Cursor;
pub use deepseek::DeepSeek;
pub use elevenlabs::ElevenLabs;
pub use gemini::Gemini;
pub use groq::Groq;
pub use kimi::Kimi;
pub use minimax::MiniMax;
pub use opencodego::{OpenCodeGo, OpenCodePaths};
pub use openrouter::OpenRouter;
pub use pending::PendingProvider;
pub use xai::Xai;
pub use zai::Zai;
