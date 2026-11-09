//! Guard: no observation races the step that changes what it observes.
//!
//! The runner waits for state, never for time (`oracle/README.md`, *Races
//! under load*), but only where the scenario says what to wait for. A
//! `screen` (`title`, `progress`) settles first: 150 ms with no output, and
//! before that the reaction to the keys, `feed`, `resize` or `control_send`
//! just before it. A CLI step provokes nothing the runner can wait for on its
//! own, so on a loaded host 150 ms can pass before the server has redrawn for
//! the command, and the screen is taken from before it. Three races:
//!
//! - a `screen` after a `run` that changes something, with no reacting step
//!   between: `settle { react = true }` (output since the last settle), or
//!   `wait_screen` for what the run brings about, or input whose reaction the
//!   observation waits for (its frame shows the run's state too: the server
//!   ran the command before it read the input);
//! - a `run` (or `feed`) right after `keys`: the attached client forwards the
//!   keys on its own time, so the CLI command can reach the server first;
//!   `settle` between them;
//! - a `capture-pane` after input to a pane (`feed`, `send-keys`,
//!   `paste-buffer`): the pane program answers on its own time;
//!   `wait_for` a signal from it, `wait_pane` or `wait_screen` for its
//!   output, or a `settle` that reacts to it.
//!
//! A `run` that only reads state (`display -p`, `show-*`, `list-*`,
//! `capture-pane -p`, `has-session`) changes nothing to race.

use super::scenario::{Scenario, Step};

/// Commands that only print state, whatever their flags.
const QUERIES: &[&str] = &[
    "show-options",
    "show",
    "show-window-options",
    "showw",
    "show-environment",
    "showenv",
    "show-hooks",
    "show-buffer",
    "showb",
    "show-messages",
    "showmsgs",
    "list-sessions",
    "ls",
    "list-windows",
    "lsw",
    "list-panes",
    "lsp",
    "list-clients",
    "lsc",
    "list-keys",
    "lsk",
    "list-buffers",
    "lsb",
    "list-commands",
    "lscm",
    "has-session",
    "has",
];

/// Commands that send input to a pane's program.
const PANE_INPUT: &[&str] = &["send-keys", "send", "paste-buffer", "pasteb"];

/// The commands of a `run` argv (a `;`-separated list), each without the
/// client's global flags (`-f file`).
fn commands(argv: &[String]) -> impl Iterator<Item = &[String]> {
    argv.split(|a| a == ";").filter_map(|cmd| {
        let mut start = 0;
        while cmd.get(start).is_some_and(|a| a.starts_with('-')) {
            start += if matches!(cmd[start].as_str(), "-f" | "-L" | "-S" | "-c" | "-T") {
                2
            } else {
                1
            };
        }
        cmd.get(start..).filter(|c| !c.is_empty())
    })
}

/// The flags tmux's getopt sees in `args` for a command whose flags taking a
/// value are `with_value` (`args_parse`: stops at the first non-flag or `--`).
fn flags(args: &[String], with_value: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(flags) = args
        .get(i)
        .and_then(|a| a.strip_prefix('-'))
        .filter(|f| !f.is_empty() && *f != "-")
    {
        i += 1;
        for (j, c) in flags.char_indices() {
            out.push(c);
            if with_value.contains(c) {
                if j + c.len_utf8() == flags.len() {
                    i += 1; // the value is the next argument
                }
                break;
            }
        }
    }
    out
}

/// Whether every command of a `run` only prints state: it changes nothing a
/// later observation could race (getopt strings from cmd-display-message.c
/// and cmd-capture-pane.c; without `-p` both act on a client or a buffer).
fn is_query(argv: &[String]) -> bool {
    commands(argv).all(|cmd| {
        let (name, rest) = (cmd[0].as_str(), &cmd[1..]);
        match name {
            "display-message" | "display" => flags(rest, "cdtF").contains(&'p'),
            "capture-pane" | "capturep" => flags(rest, "bEST").contains(&'p'),
            _ => QUERIES.contains(&name),
        }
    })
}

/// Whether a `run` sends input to a pane's program (`send-keys -X` runs a
/// copy-mode command in the server, and `-K` goes to the client's key table).
fn is_pane_input(argv: &[String]) -> bool {
    commands(argv).any(|cmd| {
        PANE_INPUT.contains(&cmd[0].as_str()) && {
            let f = flags(&cmd[1..], "cNtT");
            !f.contains(&'X') && !f.contains(&'K')
        }
    })
}

fn is_capture(argv: &[String]) -> bool {
    commands(argv).any(|cmd| matches!(cmd[0].as_str(), "capture-pane" | "capturep"))
}

/// What the steps so far leave unwaited for, as the step numbers that did it.
#[derive(Default)]
struct Pending {
    /// The last `run` that changes state with nothing after it that waits
    /// for the server to have drawn it.
    run: Option<usize>,
    /// The input whose reaction the next settle waits for (the runner's
    /// `expect_after`).
    input: Option<usize>,
    /// Keys to an attached client nothing has waited for yet.
    keys: Option<usize>,
    /// Input to a pane's program nothing has waited for yet.
    pane: Option<usize>,
}

impl Pending {
    /// A settle: with `react`, for output since the last settle; otherwise for
    /// the reaction to the input, if any, then quiet.
    fn settle(&mut self, react: bool) {
        let input = self.input.take();
        let covers =
            |step: Option<usize>| react || input.is_some_and(|k| step.is_none_or(|s| k >= s));
        if covers(self.run) {
            self.run = None;
        }
        if covers(self.pane) {
            self.pane = None;
        }
        self.keys = None;
    }
}

/// Every observation in `sc` that can race the step before it, as
/// `step N: ...` (steps counted from 1).
pub fn races(sc: &Scenario) -> Vec<String> {
    let mut out = Vec::new();
    let mut p = Pending::default();
    for (i, step) in sc.steps.iter().enumerate() {
        let n = i + 1;
        match step {
            Step::Run(argv) => {
                if let Some(k) = p.keys.take() {
                    out.push(format!(
                        "step {n}: `{}` can reach the server before the keys of step {k}: \
                         put {{ settle = {{}} }} between them",
                        argv.join(" ")
                    ));
                }
                if is_capture(argv)
                    && let Some(input) = p.pane.take()
                {
                    out.push(format!(
                        "step {n}: `{}` can capture the pane before it has the output of \
                         step {input}'s input: wait for it (wait_for a signal from the program, \
                         wait_pane, wait_screen, or a settle that reacts to it)",
                        argv.join(" ")
                    ));
                }
                if !is_query(argv) {
                    p.run = Some(n);
                }
                if is_pane_input(argv) {
                    p.pane = Some(n);
                }
            }
            Step::Keys(_) | Step::KeysTo(_) => {
                p.input = Some(n);
                p.keys = Some(n);
            }
            Step::Feed(_) => {
                if let Some(k) = p.keys.take() {
                    out.push(format!(
                        "step {n}: the feed can reach the pane before the keys of step {k}: \
                         put {{ settle = {{}} }} between them"
                    ));
                }
                p.input = Some(n);
                p.pane = Some(n);
            }
            Step::Resize(_) | Step::ControlSend(_) => p.input = Some(n),
            Step::Settle(s) => p.settle(s.react),
            Step::ControlWait(_) => p.settle(false),
            Step::WaitScreen(_) => {
                p.run = None;
                p.keys = None;
                p.pane = None;
            }
            // The pane shows its program's answer.
            Step::WaitPane(_) => p.pane = None,
            // Its pane program signalled: it has the keys and wrote its output.
            Step::WaitFor(_) => {
                p.keys = None;
                p.pane = None;
            }
            Step::Screen(_) | Step::Title(_) | Step::Progress(_) => {
                if let Some(r) = p.run
                    && p.input.is_none_or(|k| k <= r)
                {
                    let what = match step {
                        Step::Screen(_) => "screen",
                        Step::Title(_) => "title",
                        _ => "progress",
                    };
                    out.push(format!(
                        "step {n}: the {what} can be observed before the server has drawn \
                         step {r}'s command: put {{ settle = {{ react = true }} }} or a \
                         wait_screen before it"
                    ));
                    p.run = None;
                }
                p.settle(false);
            }
            // An attach returns after the client's first draw, which shows
            // every command before it; an upgrade waits for the redraw.
            Step::Attach(_) => p.run = None,
            Step::Upgrade(_) => {
                p.run = None;
                p.input = None;
            }
            Step::Setup(_) | Step::Control(_) | Step::ControlClose(_) | Step::Divergence(_) => {}
        }
    }
    out
}

/// The races in every scenario under `root`, as `<scenario>: step N: ...`.
pub fn all_races(root: &std::path::Path) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for sc in super::load_scenarios(root, None)? {
        out.extend(races(&sc).into_iter().map(|r| format!("{}: {r}", sc.name)));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &[&str]) -> Vec<String> {
        s.iter().map(|&a| a.into()).collect()
    }

    fn scenario(steps: &str) -> Scenario {
        toml::from_str(&format!("name = \"t\"\nsteps = [{steps}]")).expect("scenario parses")
    }

    fn flagged(steps: &str) -> Vec<String> {
        races(&scenario(steps))
            .iter()
            .map(|r| r.split(':').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn queries_change_nothing() {
        assert!(is_query(&argv(&["display", "-p", "#{pane_id}"])));
        assert!(is_query(&argv(&["display", "-t", "a", "-pF", "x"])));
        assert!(is_query(&argv(&["capture-pane", "-p", "-t", "%0"])));
        assert!(is_query(&argv(&["lsp", ";", "show", "-gv", "@x"])));
        assert!(is_query(&argv(&["-f", "/dev/null", "has", "-t", "a"])));
        // A message on the status line, a buffer, a value that is `p`.
        assert!(!is_query(&argv(&["display", "hi"])));
        assert!(!is_query(&argv(&["display", "-t", "p", "hi"])));
        assert!(!is_query(&argv(&["capture-pane", "-t", "%0"])));
        assert!(!is_query(&argv(&["show", ";", "set", "-g", "@x", "1"])));
        assert!(!is_query(&argv(&["wait-for", "-S", "go"])));
    }

    #[test]
    fn pane_input_is_not_a_server_command() {
        assert!(is_pane_input(&argv(&["send-keys", "-t", "%0", "x"])));
        assert!(is_pane_input(&argv(&["pasteb", "-t", "%0"])));
        assert!(!is_pane_input(&argv(&["send", "-X", "-t", "%0", "cancel"])));
        assert!(!is_pane_input(&argv(&["send", "-t", "%0", "-X", "cancel"])));
        assert!(!is_pane_input(&argv(&["send-keys", "-K", "-t", "a", "F5"])));
        // `-t X`'s value is no flag.
        assert!(is_pane_input(&argv(&["send", "-t", "X", "q"])));
    }

    #[test]
    fn a_screen_after_a_command_needs_a_reacting_step() {
        assert_eq!(
            flagged(
                r#"
                { attach = { session = "a" } },
                { run = ["display-panes", "-d", "0"] },
                { screen = {} },
                { run = ["set", "-g", "status", "off"] },
                { run = ["display", "-p", "x"] },
                { title = {} },
                { run = ["clock-mode"] },
                { settle = {} },
                { screen = {} },
                { run = ["clock-mode"] },
                { keys = "x" },
                { run = ["set", "-g", "@x", "1"] },
                { screen = {} }
                "#
            ),
            ["step 3", "step 6", "step 9", "step 12", "step 13"]
        );
    }

    #[test]
    fn reacting_steps_cover_the_command() {
        assert_eq!(
            flagged(
                r##"
                { run = ["new", "-d", "-s", "a", "cat", "-"] },
                { attach = { session = "a" } },
                { screen = {} },
                { run = ["display-panes", "-d", "0"] },
                { settle = { react = true } },
                { screen = {} },
                { run = ["clock-mode"] },
                { wait_screen = { has = ["x"] } },
                { screen = {} },
                { run = ["clock-mode"] },
                { keys = "q" },
                { screen = {} },
                { run = ["resize-window", "-x", "10"] },
                { resize = [20, 10] },
                { progress = {} },
                { run = ["display", "-p", "#{pane_id}"] },
                { screen = {} },
                { run = ["set", "-g", "status", "off"] },
                { upgrade = {} },
                { screen = {} }
                "##
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_command_after_keys_waits_for_them() {
        assert_eq!(
            flagged(
                r#"
                { attach = { session = "a" } },
                { keys = "\u0003" },
                { run = ["set", "-g", "message-format", "x"] },
                { keys_to = { id = "b", keys = "q" } },
                { feed = { pane = "%0", bytes = "x" } },
                { keys = "q" },
                { settle = {} },
                { run = ["display", "-p", "x"] },
                { keys = "e" },
                { wait_for = "opened" },
                { run = ["display", "-p", "x"] }
                "#
            ),
            ["step 3", "step 5"]
        );
    }

    #[test]
    fn a_capture_after_pane_input_waits_for_the_program() {
        assert_eq!(
            flagged(
                r#"
                { run = ["send-keys", "-t", "%0", "x"] },
                { run = ["capture-pane", "-p"] },
                { feed = { pane = "%0", bytes = "y" } },
                { run = ["capturep", "-p"] },
                { feed = { pane = "%0", bytes = "y" } },
                { settle = {} },
                { run = ["capturep", "-p"] },
                { run = ["send", "-t", "%0", "z"] },
                { wait_for = "seen" },
                { run = ["capturep", "-p"] },
                { run = ["send", "-X", "-t", "%0", "cancel"] },
                { run = ["capturep", "-p"] },
                { run = ["send", "-t", "%0", "z"] },
                { settle = {} },
                { run = ["capturep", "-p"] },
                { run = ["send", "-t", "%0", "z"] },
                { wait_pane = { pane = "%0", has = "z" } },
                { run = ["capturep", "-p"] }
                "#
            ),
            ["step 2", "step 4", "step 15"]
        );
    }

    #[test]
    fn no_scenario_observes_before_the_step_it_waits_for() {
        let races = all_races(&super::super::workspace_root()).expect("scenarios load");
        assert!(
            races.is_empty(),
            "observations that can race the step before them on a loaded host \
             (oracle/README.md, *Races under load*):\n{}",
            races.join("\n")
        );
    }
}
