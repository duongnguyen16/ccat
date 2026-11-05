//! One module per provider: its login, model list, usage and wire translation live together here.
//! `proxy` (the parent of everything that serves a request) only dispatches by `Upstream`.

pub(crate) mod antigravity;
pub(crate) mod chatgpt;
pub(crate) mod claude;
pub(crate) mod endpoint;
pub(crate) mod gemini_web;
pub(crate) mod ollama;
pub(crate) mod openai;
pub(crate) mod opencode;

use serde::{Deserialize, Serialize};

/// The built-in profile's fixed name; every other profile is an Anthropic-compatible endpoint.
pub const AG_NAME: &str = "Antigravity";
/// The built-in ChatGPT provider's fixed name.
pub const GPT_NAME: &str = "ChatGPT";
/// What the user sees for a stored provider name: the ChatGPT provider is shown as Codex; the stored name never changes.
pub(crate) fn label(name: &str) -> &str {
    if name == GPT_NAME { "Codex" } else { name }
}
/// The built-in Ollama provider's fixed name.
pub const OLLAMA_NAME: &str = "Ollama";
/// The built-in Gemini Web provider's fixed name.
pub const GEMINI_WEB_NAME: &str = "Gemini Web";

/// The kind of provider a route runs on. Official Claude is not a `Provider` record: it is a route with an empty
/// provider name (`proxy::Upstream::Official`), so `Provider::kind` never answers `Claude`.
#[allow(dead_code)] // `Claude` is only named, never built; the Settings sidebar and pages match on the rest
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Claude,
    Antigravity,
    ChatGpt,
    Ollama,
    OpenCode,
    GeminiWeb,
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
    /// ChatGPT mode: `Some(true)` = Codex (Legacy) (`chatgpt.com/backend-api/codex`), `Some(false)` = Official
    /// (`api.openai.com`). `None` (a config from before the modes) is decided by `gpt_legacy`.
    pub(crate) chatgpt_legacy: Option<bool>,
    /// What the last model fetch learned of each ChatGPT model; `models` holds just the slugs.
    pub(crate) chatgpt_models: Vec<chatgpt::ChatgptModel>,
    /// Served through the local proxy from local Ollama instance (`/api/chat`).
    pub(crate) ollama: bool,
    /// Served through the local Argon / Gemini Web bridge (`/claude/v1`).
    pub(crate) gemini_web: bool,
    /// An endpoint that speaks OpenAI Chat Completions (`url` is its versioned base) instead of Anthropic Messages.
    pub(crate) openai: bool,
    /// Anonymous OpenCode Free proxy, not a regular keyed endpoint.
    pub(crate) opencode: bool,
    /// OpenCode egress pool configuration.
    #[serde(default)]
    pub(crate) opencode_egress: opencode::egress::Config,
    /// Fetch keeps only the models free by their name (see `openai::is_free`).
    pub(crate) free_only: bool,
    /// The retired MuseAI built-in, read from old configs only so `load` can drop it.
    #[serde(skip_serializing)]
    pub(crate) muse: bool,
    /// Turned off in the Providers view: not offered in the pickers, not used for routing.
    pub(crate) disabled: bool,
}

impl Provider {
    /// Whether ChatGPT runs in Codex (Legacy) mode. Unset: Legacy only when an account that can run there exists (a
    /// Codex-client sign-in); a Codex CLI import left by an older ccat has no token, so it does not count, and a new
    /// config is Official.
    pub(crate) fn gpt_legacy(&self) -> bool {
        self.chatgpt_legacy.unwrap_or_else(|| self.chatgpt_accounts.iter().any(|a| a.own_codex()))
    }

    pub(crate) fn is_opencode(&self) -> bool {
        self.opencode
    }

    /// Which provider this record is, from its flags (none set: an Anthropic-compatible endpoint).
    pub(crate) fn kind(&self) -> Kind {
        if self.antigravity {
            Kind::Antigravity
        } else if self.chatgpt {
            Kind::ChatGpt
        } else if self.ollama {
            Kind::Ollama
        } else if self.is_opencode() {
            Kind::OpenCode
        } else if self.gemini_web {
            Kind::GeminiWeb
        } else {
            Kind::Endpoint
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chatgpt_provider_is_shown_as_codex_and_stored_as_chatgpt() {
        assert_eq!((label(GPT_NAME), GPT_NAME), ("Codex", "ChatGPT"));
        for same in [AG_NAME, OLLAMA_NAME, "", "Claude", "chatgpt", "My Codex endpoint"] {
            assert_eq!(label(same), same, "only the stored ChatGPT name is renamed");
        }
    }
}
