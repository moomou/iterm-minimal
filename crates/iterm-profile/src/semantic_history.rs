use plist::Dictionary;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Semantic History action configured in the iTerm2 profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SemanticHistoryAction {
    BestEditor,
    DefaultApp,
    CustomEditor(String),
    RunCommand(String),
    OpenUrl(String),
}

impl Default for SemanticHistoryAction {
    fn default() -> Self {
        Self::BestEditor
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SemanticHistoryConfig {
    pub action: SemanticHistoryAction,
    pub editor_bundle_id: Option<String>,
    pub command_template: String,
}

impl Default for SemanticHistoryConfig {
    fn default() -> Self {
        Self {
            action: SemanticHistoryAction::BestEditor,
            editor_bundle_id: None,
            command_template: String::new(),
        }
    }
}

impl SemanticHistoryConfig {
    pub fn from_iterm_plist_dict(dict: &Dictionary) -> Self {
        let action_str = dict.get("action").and_then(|v| v.as_string()).unwrap_or("best editor");
        let editor = dict
            .get("editor")
            .and_then(|v| v.as_string())
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned);
        let text = dict
            .get("text")
            .and_then(|v| v.as_string())
            .unwrap_or("")
            .to_string();
        let action = match action_str {
            "best editor" => SemanticHistoryAction::BestEditor,
            "default" => SemanticHistoryAction::DefaultApp,
            "editor" => SemanticHistoryAction::CustomEditor(
                editor.clone().unwrap_or_default(),
            ),
            "command" | "raw action" => SemanticHistoryAction::RunCommand(text.clone()),
            "url" => SemanticHistoryAction::OpenUrl(text.clone()),
            _ => SemanticHistoryAction::BestEditor,
        };
        Self {
            action,
            editor_bundle_id: editor,
            command_template: text,
        }
    }
}

/// Explicit remote artifact stamped onto a terminal line by shell integration / `it2artifact`
/// via `OSC 1337;Artifact=...` (also works seamlessly through tmux DCS passthrough).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteArtifact {
    /// Connection identity (`user@host` or SSH alias / ControlMaster target).
    pub host: String,
    /// Optional SSH port override.
    pub port: Option<u16>,
    /// Canonical absolute remote path.
    pub abs_path: String,
    /// Optional connection/session identifier.
    pub conn_id: Option<String>,
    /// Optional kind hint (`file`, `image`, `dir`).
    pub kind: Option<String>,
    /// Optional byte size hint.
    pub size: Option<u64>,
    /// Column span `[start_col, end_col)` on the screen row where this artifact is clickable.
    pub start_col: usize,
    pub end_col: usize,
}

/// Resolved Cmd+Click target produced by Semantic History.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticTarget {
    /// Open URL in default browser (from OSC 8 hyperlink or detected URL).
    Url(String),
    /// Deterministic remote artifact (from explicit `OSC 1337;Artifact=`).
    ExplicitRemoteArtifact(RemoteArtifact),
    /// Inferred remote file path using line's `RemoteHost` + `CurrentDir` metadata (SCP fallback).
    InferredRemoteFile {
        host: String,
        remote_path: String,
        line: Option<u32>,
        col: Option<u32>,
    },
    /// Local filesystem file path (resolved relative to line's `CurrentDir` if relative).
    LocalFile {
        path: PathBuf,
        line: Option<u32>,
        col: Option<u32>,
    },
}

/// Split an optional `:line` or `:line:col` suffix from a clicked path token.
pub fn split_path_line_col(token: &str) -> (&str, Option<u32>, Option<u32>) {
    let trimmed = token
        .trim()
        .trim_matches(|c| matches!(c, '\'' | '"' | '`' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';'));
    let parts: Vec<&str> = trimmed.rsplitn(3, ':').collect();
    if parts.len() == 3 {
        if let (Ok(col), Ok(line)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) {
            return (parts[2], Some(line), Some(col));
        }
    }
    if parts.len() >= 2 {
        if let Ok(line) = parts[0].parse::<u32>() {
            let base = &trimmed[..trimmed.len() - parts[0].len() - 1];
            return (base, Some(line), None);
        }
    }
    (trimmed, None, None)
}

/// Classify a Cmd+Click token using explicit artifacts, OSC 8 URL, or RemoteHost/CurrentDir metadata.
pub fn resolve_semantic_target(
    token: &str,
    osc8_url: Option<&str>,
    explicit_artifact: Option<&RemoteArtifact>,
    remote_host: Option<&str>,
    local_hostname: &str,
    current_dir: Option<&str>,
) -> Option<SemanticTarget> {
    if let Some(art) = explicit_artifact {
        return Some(SemanticTarget::ExplicitRemoteArtifact(art.clone()));
    }
    if let Some(url) = osc8_url {
        if !url.is_empty() {
            return Some(SemanticTarget::Url(url.to_string()));
        }
    }

    let (clean, line, col) = split_path_line_col(token);
    if clean.is_empty() {
        return None;
    }

    if clean.starts_with("http://")
        || clean.starts_with("https://")
        || clean.starts_with("mailto:")
        || clean.starts_with("file://")
    {
        return Some(SemanticTarget::Url(clean.to_string()));
    }

    // Check whether this line came from a remote host via shell integration RemoteHost.
    let is_remote = remote_host.is_some_and(|rh| {
        let host_part = rh.split('@').next_back().unwrap_or(rh);
        !host_part.is_empty()
            && host_part != "localhost"
            && host_part != "127.0.0.1"
            && !host_part.eq_ignore_ascii_case(local_hostname)
    });

    if is_remote {
        let host = remote_host.unwrap().to_string();
        let remote_path = if clean.starts_with('/') || clean.starts_with('~') {
            clean.to_string()
        } else if let Some(cwd) = current_dir {
            let cwd = cwd.trim_end_matches('/');
            let rel = clean.strip_prefix("./").unwrap_or(clean);
            format!("{cwd}/{rel}")
        } else {
            clean.to_string()
        };
        return Some(SemanticTarget::InferredRemoteFile {
            host,
            remote_path,
            line,
            col,
        });
    }

    // Local path resolution.
    let path = if clean.starts_with('/') {
        PathBuf::from(clean)
    } else if let Some(rest) = clean.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else if let Some(cwd) = current_dir {
        PathBuf::from(cwd).join(clean.strip_prefix("./").unwrap_or(clean))
    } else {
        PathBuf::from(clean)
    };

    Some(SemanticTarget::LocalFile { path, line, col })
}
