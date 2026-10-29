//! Scenario schema (`oracle/scenarios/*.toml`) and the observations a run produces.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// A declarative scenario: inputs only, never expected output.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    /// Attached-client size `[cols, rows]`.
    #[serde(default = "default_size")]
    pub size: [u16; 2],
    pub steps: Vec<Step>,
}

fn default_size() -> [u16; 2] {
    [80, 24]
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    /// CLI invocation; observes stdout, stderr and the exit code.
    Run(Vec<String>),
    Setup(Setup),
    /// Start an attached `-C` client that stays alive.
    Control(ControlStart),
    ControlSend(ControlSend),
    ControlWait(ControlWait),
    /// EOF on stdin, read to `%exit`; observes the whole stream.
    ControlClose(ControlId),
    /// Start an attached client under a PTY of the scenario size.
    Attach(AttachSpec),
    /// Raw bytes to the attached client.
    Keys(String),
    /// Bytes into a pane through `send-keys -l`.
    Feed(Feed),
    Resize([u16; 2]),
    /// No output from any client for `quiet_ms` (default 150).
    Settle(Settle),
    /// Run `wait-for <channel>`.
    WaitFor(String),
    /// Observe the attached client's screen.
    Screen(Empty),
    /// Annotates the next observation: replay compares vtmux against `expect`.
    Divergence(Divergence),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    /// A socket file that was bound and then closed at the scenario's socket path.
    StaleSocket,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlStart {
    pub argv: Vec<String>,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSend {
    pub id: String,
    pub line: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlWait {
    pub id: String,
    /// Line prefix such as `%end` or `%output`, looked for after the last send.
    pub until: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlId {
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AttachSpec {
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Feed {
    pub pane: String,
    pub bytes: String,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Divergence {
    pub id: String,
    /// vtmux's documented behaviour, in normalised form.
    pub expect: Observation,
}

/// One normalised observation.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observation {
    Cli {
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        exit: i32,
    },
    Control {
        lines: Vec<String>,
        #[serde(default)]
        stderr: String,
        exit: i32,
    },
    Screen {
        rows: Vec<String>,
        cursor: [u16; 2],
    },
    /// A wait that never completed.
    Timeout {
        what: String,
    },
}

pub fn load(path: &Path) -> Result<Scenario, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sc: Scenario = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if sc.name != stem {
        return Err(format!(
            "{}: name {:?} must match the file name",
            path.display(),
            sc.name
        )
        .into());
    }
    Ok(sc)
}
