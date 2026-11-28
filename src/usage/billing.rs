//! Which billing a harness session runs under, from the config or from what is present on
//! this machine. Detection reads only key presence or one mode string; a credential is never
//! read into a value that is stored, logged or printed.

use std::path::Path;

use serde::Deserialize;

use crate::client::UsageConfig;
use crate::domain::Billing;
use crate::hook::Harness;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Config,
    Detected,
}

const CLAUDE_API_ENV: [&str; 5] = [
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    "CLAUDE_CODE_USE_FOUNDRY",
];

/// `[usage.billing]` for this harness, else detection.
pub fn resolve(
    h: Harness,
    home: &Path,
    env: &dyn Fn(&str) -> Option<String>,
    cfg: &UsageConfig,
) -> (Billing, Source) {
    match cfg.billing_for(h.name()) {
        Some(b) => (b, Source::Config),
        None => (detect(h, home, env), Source::Detected),
    }
}

pub fn detect(h: Harness, home: &Path, env: &dyn Fn(&str) -> Option<String>) -> Billing {
    match h {
        Harness::ClaudeCode => detect_claude(home, env),
        Harness::Codex => detect_codex(home),
    }
}

#[derive(Deserialize)]
struct ClaudeSettings {
    #[serde(rename = "apiKeyHelper")]
    api_key_helper: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct ClaudeState {
    #[serde(rename = "oauthAccount")]
    oauth_account: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CodexAuth {
    auth_mode: Option<String>,
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn detect_claude(home: &Path, env: &dyn Fn(&str) -> Option<String>) -> Billing {
    if CLAUDE_API_ENV
        .iter()
        .any(|k| env(k).is_some_and(|v| !v.is_empty()))
    {
        return Billing::Api;
    }
    let helper = read_json::<ClaudeSettings>(&home.join(".claude").join("settings.json"))
        .and_then(|s| s.api_key_helper)
        .is_some();
    if helper {
        return Billing::Api;
    }
    let oauth = read_json::<ClaudeState>(&home.join(".claude.json"))
        .and_then(|s| s.oauth_account)
        .is_some_and(|v| v.is_object());
    if oauth {
        Billing::Subscription
    } else {
        Billing::Unknown
    }
}

fn detect_codex(home: &Path) -> Billing {
    let mode =
        read_json::<CodexAuth>(&home.join(".codex").join("auth.json")).and_then(|a| a.auth_mode);
    match mode.as_deref() {
        Some("chatgpt") => Billing::Subscription,
        Some("apikey") => Billing::Api,
        _ => Billing::Unknown,
    }
}
