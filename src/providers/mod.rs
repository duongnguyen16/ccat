//! One module per provider: its login, model list, usage and wire translation live together here.
//! `proxy` (the parent of everything that serves a request) only dispatches by `Upstream`.

pub(crate) mod antigravity;
pub(crate) mod chatgpt;
pub(crate) mod claude;
pub(crate) mod endpoint;
pub(crate) mod museai;
pub(crate) mod ollama;

use serde::{Deserialize, Serialize};

/// The built-in profile's fixed name; every other profile is an Anthropic-compatible endpoint.
pub const AG_NAME: &str = "Antigravity";
/// The built-in ChatGPT provider's fixed name.
pub const GPT_NAME: &str = "ChatGPT";
/// The built-in Ollama provider's fixed name.
pub const OLLAMA_NAME: &str = "Ollama";
/// The built-in MuseAI provider's fixed name.
pub const MUSE_NAME: &str = "MuseAI";

/// The kind of provider a route runs on. Official Claude is not a `Provider` record: it is a route with an empty
/// provider name (`proxy::Upstream::Official`), so `Provider::kind` never answers `Claude`.
#[allow(dead_code)] // ponytail: the next phases replace the `if p.antigravity …` chains with it
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Claude,
    Antigravity,
    ChatGpt,
    Ollama,
    Muse,
    Endpoint,
}

/// A connection: an Anthropic-compatible endpoint (`url` + `key`) or the built-in Antigravity account pool.
#[derive(Serialize, Deserialize, Default, Clone)]
#[serde(default)]
pub(crate) struct Provider {
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) key: String,
    pub(crate) models: Vec<String>,
    pub(crate) custom: Vec<String>,
    /// Served through the local proxy from `accounts` instead of `url` + `key`.
    pub(crate) antigravity: bool,
    pub(crate) accounts: Vec<antigravity::Account>,
    /// Served through the local proxy from the ChatGPT plan (`chatgpt_accounts`; their tokens live in files, see `chatgpt`).
    pub(crate) chatgpt: bool,
    pub(crate) chatgpt_accounts: Vec<chatgpt::ChatgptAccount>,
    /// What the last model fetch learned of each ChatGPT model; `models` holds just the slugs.
    pub(crate) chatgpt_models: Vec<chatgpt::ChatgptModel>,
    /// Served through the local proxy from local Ollama instance (`/v1/chat/completions`).
    pub(crate) ollama: bool,
    /// Served through the local proxy from MuseAI via local sidecar engine.
    pub(crate) muse: bool,
    #[serde(default)]
    pub(crate) muse_account: Option<museai::Account>,
    /// Turned off in the Providers view: not offered in the pickers, not used for routing.
    pub(crate) disabled: bool,
}

impl Provider {
    /// Which provider this record is, from its flags (none set: an Anthropic-compatible endpoint).
    #[allow(dead_code)] // ponytail: see `Kind`
    pub(crate) fn kind(&self) -> Kind {
        if self.antigravity {
            Kind::Antigravity
        } else if self.chatgpt {
            Kind::ChatGpt
        } else if self.ollama {
            Kind::Ollama
        } else if self.muse {
            Kind::Muse
        } else {
            Kind::Endpoint
        }
    }
}
