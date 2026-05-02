use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStore {
    pub groups: Vec<SessionGroup>,
    pub presets: Vec<AgentPreset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionGroup {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub sessions: Vec<SessionMeta>,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub name: String,
    pub command: Vec<String>,
    pub working_dir: PathBuf,
    pub socket_path: PathBuf,
    pub preset: Option<String>,
    /// Agent's own session/conversation ID (if different from our session name).
    /// Used for resume when the agent assigns its own IDs.
    pub agent_session_id: Option<String>,
    pub status: SessionStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Running,
    Dead,
    Archived,
}

/// Defines how an agent is launched, resumed, and configured.
///
/// Agents are defined declaratively — the session manager uses these
/// definitions to construct the right command for launch vs resume,
/// and to understand what state can survive a process restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPreset {
    pub name: String,
    pub description: Option<String>,

    /// Base binary to invoke (e.g. "claude", "codex", "gemini")
    pub binary: String,

    /// Arguments for a fresh launch
    pub launch_args: Vec<String>,

    /// How this agent handles session resumption after process death
    pub resume: ResumeStrategy,

    /// Environment variables to set when launching
    #[serde(default)]
    pub env: Vec<(String, String)>,
}

/// Describes how an agent can recover a conversation after the process dies.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResumeStrategy {
    /// Agent has no resume capability. Just relaunch fresh.
    None {
        args: Vec<String>,
    },

    /// Agent can continue the last conversation in a given working directory.
    /// e.g. `claude --continue`
    ContinueInDirectory {
        args: Vec<String>,
    },

    /// Agent can resume a specific session by ID.
    /// e.g. `claude --resume <session_id>`
    /// The placeholder `{session_id}` in args is replaced at runtime.
    ResumeById {
        args: Vec<String>,
        /// How to obtain the session ID. If None, we use our own session name.
        session_id_source: SessionIdSource,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIdSource {
    /// Use our pty_server session name as the agent's session ID
    /// (requires the agent to accept named sessions at launch)
    SessionName,
    /// The agent assigns its own ID; we capture it from launch output
    AgentAssigned,
}

impl AgentPreset {
    /// Build the command to launch a fresh session.
    pub fn launch_command(&self) -> Vec<String> {
        let mut command = vec![self.binary.clone()];
        command.extend(self.launch_args.iter().cloned());
        command
    }

    /// Build the command to resume/continue a dead session.
    /// Returns None if the agent doesn't support resume.
    pub fn resume_command(&self, session_name: &str, _working_dir: &Path) -> Option<Vec<String>> {
        let mut command = vec![self.binary.clone()];

        match &self.resume {
            ResumeStrategy::None { args } => {
                command.extend(args.iter().cloned());
                return Some(command);
            }
            ResumeStrategy::ContinueInDirectory { args } => {
                command.extend(args.iter().cloned());
            }
            ResumeStrategy::ResumeById { args, .. } => {
                command.extend(
                    args.iter()
                        .map(|arg| arg.replace("{session_id}", session_name)),
                );
            }
        }

        Some(command)
    }

    pub fn can_resume(&self) -> bool {
        !matches!(self.resume, ResumeStrategy::None { .. })
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self {
            groups: Vec::new(),
            presets: builtin_presets(),
        }
    }
}

fn builtin_presets() -> Vec<AgentPreset> {
    vec![
        AgentPreset {
            name: "Claude Code".into(),
            description: Some("Claude Code with auto-approve".into()),
            binary: "claude".into(),
            launch_args: vec![
                "--dangerously-skip-permissions".into(),
                "--name".into(),
                "{session_id}".into(),
            ],
            resume: ResumeStrategy::ResumeById {
                args: vec![
                    "--dangerously-skip-permissions".into(),
                    "--resume".into(),
                    "{session_id}".into(),
                ],
                session_id_source: SessionIdSource::SessionName,
            },
            env: vec![],
        },
        AgentPreset {
            name: "Claude Code (safe)".into(),
            description: Some("Claude Code with manual approval".into()),
            binary: "claude".into(),
            launch_args: vec![
                "--name".into(),
                "{session_id}".into(),
            ],
            resume: ResumeStrategy::ResumeById {
                args: vec![
                    "--resume".into(),
                    "{session_id}".into(),
                ],
                session_id_source: SessionIdSource::SessionName,
            },
            env: vec![],
        },
        AgentPreset {
            name: "Codex".into(),
            description: Some("OpenAI Codex agent".into()),
            binary: "codex".into(),
            launch_args: vec![
                "--ask-for-approval".into(),
                "never".into(),
                "--sandbox".into(),
                "danger-full-access".into(),
            ],
            resume: ResumeStrategy::None {
                args: vec![
                    "--ask-for-approval".into(),
                    "never".into(),
                    "--sandbox".into(),
                    "danger-full-access".into(),
                ],
            },
            env: vec![],
        },
        AgentPreset {
            name: "Gemini".into(),
            description: Some("Google Gemini CLI".into()),
            binary: "gemini".into(),
            launch_args: vec!["-y".into()],
            resume: ResumeStrategy::ContinueInDirectory {
                args: vec!["-y".into()],
            },
            env: vec![],
        },
        AgentPreset {
            name: "Copilot".into(),
            description: Some("GitHub Copilot CLI".into()),
            binary: "copilot".into(),
            launch_args: vec!["--allow-all".into()],
            resume: ResumeStrategy::None {
                args: vec!["--allow-all".into()],
            },
            env: vec![],
        },
        AgentPreset {
            name: "Amp".into(),
            description: Some("Amp agent".into()),
            binary: "amp".into(),
            launch_args: vec![],
            resume: ResumeStrategy::None { args: vec![] },
            env: vec![],
        },
        AgentPreset {
            name: "Terminal".into(),
            description: Some("Plain terminal session".into()),
            binary: std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()),
            launch_args: vec![],
            resume: ResumeStrategy::None { args: vec![] },
            env: vec![],
        },
    ]
}

impl SessionStore {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = fs::read_to_string(path)?;
        let store: Self = serde_json::from_str(&content)?;
        Ok(store)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        fs::write(path, content)?;
        Ok(())
    }

    pub fn create_group(&mut self, name: &str, path: PathBuf) -> &SessionGroup {
        let group = SessionGroup {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            path,
            sessions: Vec::new(),
            created_at: chrono_now(),
        };
        self.groups.push(group);
        self.groups.last().expect("just pushed")
    }

    pub fn find_group(&self, id: &str) -> Option<&SessionGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub fn find_group_mut(&mut self, id: &str) -> Option<&mut SessionGroup> {
        self.groups.iter_mut().find(|group| group.id == id)
    }

    pub fn add_session(
        &mut self,
        group_id: &str,
        name: &str,
        command: Vec<String>,
        working_dir: PathBuf,
        socket_path: PathBuf,
        preset: Option<String>,
    ) -> anyhow::Result<&SessionMeta> {
        let group = self
            .find_group_mut(group_id)
            .ok_or_else(|| anyhow::anyhow!("group not found: {group_id}"))?;

        let session = SessionMeta {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            command,
            working_dir,
            socket_path,
            preset,
            agent_session_id: None,
            status: SessionStatus::Running,
            created_at: chrono_now(),
        };

        group.sessions.push(session);
        Ok(group.sessions.last().expect("just pushed"))
    }

    /// Check all sessions and update their status based on socket liveness.
    pub fn refresh_statuses(&mut self) {
        for group in &mut self.groups {
            for session in &mut group.sessions {
                if session.status == SessionStatus::Archived {
                    continue;
                }
                if std::os::unix::net::UnixStream::connect(&session.socket_path).is_ok() {
                    session.status = SessionStatus::Running;
                } else {
                    session.status = SessionStatus::Dead;
                }
            }
        }
    }

    pub fn find_preset(&self, name: &str) -> Option<&AgentPreset> {
        self.presets.iter().find(|preset| preset.name == name)
    }
}

fn chrono_now() -> String {
    // Simple ISO-ish timestamp without pulling in chrono crate
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_default_has_presets() {
        let store = SessionStore::default();
        assert!(store.presets.len() >= 3);
        assert!(store.find_preset("Claude Code").is_some());
        assert!(store.find_preset("Terminal").is_some());
    }

    #[test]
    fn test_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let mut store = SessionStore::default();
        store.create_group("my project", PathBuf::from("/home/user/project"));

        store.save(&path).unwrap();

        let loaded = SessionStore::load(&path).unwrap();
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.groups[0].name, "my project");
        assert_eq!(
            loaded.groups[0].path,
            PathBuf::from("/home/user/project")
        );
    }

    #[test]
    fn test_create_group_and_add_session() {
        let mut store = SessionStore::default();
        let group = store.create_group("test group", PathBuf::from("/tmp"));
        let group_id = group.id.clone();

        store
            .add_session(
                &group_id,
                "claude-1",
                vec!["claude".into()],
                PathBuf::from("/tmp"),
                PathBuf::from("/tmp/claude-1.sock"),
                Some("Claude Code".into()),
            )
            .unwrap();

        let group = store.find_group(&group_id).unwrap();
        assert_eq!(group.sessions.len(), 1);
        assert_eq!(group.sessions[0].name, "claude-1");
        assert_eq!(group.sessions[0].status, SessionStatus::Running);
    }

    #[test]
    fn test_add_session_bad_group() {
        let mut store = SessionStore::default();
        let result = store.add_session(
            "nonexistent",
            "test",
            vec!["bash".into()],
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/test.sock"),
            None,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_refresh_statuses() {
        let mut store = SessionStore::default();
        let group = store.create_group("test", PathBuf::from("/tmp"));
        let group_id = group.id.clone();

        store
            .add_session(
                &group_id,
                "dead-session",
                vec!["bash".into()],
                PathBuf::from("/tmp"),
                PathBuf::from("/tmp/nonexistent.sock"),
                None,
            )
            .unwrap();

        store.refresh_statuses();

        let group = store.find_group(&group_id).unwrap();
        assert_eq!(group.sessions[0].status, SessionStatus::Dead);
    }

    #[test]
    fn test_claude_launch_command() {
        let store = SessionStore::default();
        let preset = store.find_preset("Claude Code").unwrap();
        let command = preset.launch_command();
        assert_eq!(command[0], "claude");
        assert!(command.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(command.contains(&"--name".to_string()));
    }

    #[test]
    fn test_claude_resume_command() {
        let store = SessionStore::default();
        let preset = store.find_preset("Claude Code").unwrap();
        assert!(preset.can_resume());

        let command = preset.resume_command("my-session", Path::new("/tmp")).unwrap();
        assert_eq!(command[0], "claude");
        assert!(command.contains(&"--resume".to_string()));
        assert!(command.contains(&"my-session".to_string()));
    }

    #[test]
    fn test_terminal_no_resume() {
        let store = SessionStore::default();
        let preset = store.find_preset("Terminal").unwrap();
        assert!(!preset.can_resume());
    }

    #[test]
    fn test_codex_no_resume() {
        let store = SessionStore::default();
        let preset = store.find_preset("Codex").unwrap();
        assert!(!preset.can_resume());
        let command = preset.launch_command();
        assert_eq!(command[0], "codex");
    }

    #[test]
    fn test_gemini_continue_in_directory() {
        let store = SessionStore::default();
        let preset = store.find_preset("Gemini").unwrap();
        assert!(preset.can_resume());
        let command = preset.resume_command("whatever", Path::new("/my/project")).unwrap();
        assert_eq!(command[0], "gemini");
        assert!(command.contains(&"-y".to_string()));
    }

    #[test]
    fn test_preset_roundtrip_serialization() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");

        let store = SessionStore::default();
        store.save(&path).unwrap();

        let loaded = SessionStore::load(&path).unwrap();
        assert_eq!(loaded.presets.len(), store.presets.len());

        let claude = loaded.find_preset("Claude Code").unwrap();
        assert!(claude.can_resume());
        let command = claude.resume_command("test-id", Path::new("/tmp")).unwrap();
        assert!(command.contains(&"test-id".to_string()));
    }

    #[test]
    fn test_load_nonexistent_returns_default() {
        let store = SessionStore::load(Path::new("/tmp/nonexistent_sessions.json")).unwrap();
        assert!(store.groups.is_empty());
        assert!(!store.presets.is_empty());
    }
}
