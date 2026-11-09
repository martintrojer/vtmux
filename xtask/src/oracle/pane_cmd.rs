//! Guard: no scenario starts a pane with a one-word command.
//!
//! tmux runs a one-word command as `$SHELL -c <word>` (spawn.c `spawn_pane`).
//! bash execs the last command, so `#{pane_current_command}` turns into the
//! command's name, but Ubuntu's `/bin/sh` is dash, which forks it and stays
//! `sh` in front. A screen showing the name then differs per host. Two or more
//! words are `execvp`'d with no shell, so the name is right at once on every
//! host. A pane command is given as separate arguments (`cat`, `-`), never one
//! string (`"cat -"` is still one shell command).

use super::scenario::{Scenario, Step};

/// Commands that start a process in a pane, with their flags that take a value
/// (the getopt strings of cmd-new-session.c, cmd-new-window.c,
/// cmd-split-window.c and cmd-respawn-{pane,window}.c).
const PANE_COMMANDS: &[(&[&str], &str)] = &[
    (&["new-session", "new"], "Ac:dDe:EF:f:n:Ps:t:x:Xy:"),
    (&["new-window", "neww"], "abc:de:EF:kn:PSt:"),
    (
        &["split-window", "splitw"],
        "bB:c:de:EfF:hIkl:m:p:PR:s:S:t:T:vWZ",
    ),
    (
        &["new-pane", "newp"],
        "AbB:Cc:Dde:EfF:hIkl:KLMm:Op:PR:s:S:t:T:vWx:X:y:Y:Z",
    ),
    (&["respawn-pane", "respawnp"], "c:e:Ekt:"),
    (&["respawn-window", "respawnw"], "c:e:Ekt:"),
];

/// Commands whose arguments can be tmux command lines, which are checked too.
const TAKES_COMMANDS: &[&str] = &[
    "set-hook",
    "bind-key",
    "bind",
    "if-shell",
    "if",
    "confirm-before",
    "confirm",
    "command-prompt",
    "display-menu",
    "menu",
];

/// A single-argument command that is a script, or ends the shell: no
/// metacharacter-free simple command for the shell to run as a child (`exec cat`
/// replaces the shell, `exit 3` is a builtin, `a; exec cat` is a script the
/// scenario has to synchronise on anyway).
fn shell_script(word: &str) -> bool {
    word.is_empty()
        || word.contains(|c| ";&|<>()$`\n".contains(c))
        || word
            .split_whitespace()
            .next()
            .is_some_and(|w| matches!(w, "exec" | "exit"))
}

/// Every pane-starting command in `sc` whose command is a single word, as
/// `step N: <command>`.
pub fn one_word_panes(sc: &Scenario) -> Vec<String> {
    let mut out = Vec::new();
    for (i, step) in sc.steps.iter().enumerate() {
        let lists: Vec<Vec<String>> = match step {
            Step::Run(argv) => vec![argv.clone()],
            Step::Control(c) => vec![c.argv.clone()],
            Step::ControlSend(s) => vec![tokenize(&s.line)],
            _ => continue,
        };
        for argv in lists {
            for bad in check(&argv) {
                out.push(format!("step {}: {bad}", i + 1));
            }
        }
    }
    out
}

/// The one-word pane commands in every scenario under `root`, as
/// `<scenario>: step N: <command>`.
pub fn all_one_word_panes(
    root: &std::path::Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for sc in super::load_scenarios(root, None)? {
        out.extend(
            one_word_panes(&sc)
                .into_iter()
                .map(|o| format!("{}: {o}", sc.name)),
        );
    }
    Ok(out)
}

/// `argv` is a command or a `;`-separated list (a `-f file new ...` client
/// prefix is skipped). Strings that are command lines themselves (hooks,
/// `if-shell`, `bind-key`) are checked the same way.
fn check(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for cmd in argv.split(|a| a == ";") {
        // Global client flags before the command name (`-f file`, `-L x`).
        let mut start = 0;
        while cmd.get(start).is_some_and(|a| a.starts_with('-')) {
            start += if matches!(cmd[start].as_str(), "-f" | "-L" | "-S" | "-c" | "-T") {
                2
            } else {
                1
            };
        }
        let Some(name) = cmd.get(start) else { continue };
        let rest = &cmd[start + 1..];
        if let Some((_, spec)) = PANE_COMMANDS
            .iter()
            .find(|(aliases, _)| aliases.contains(&name.as_str()))
            && let [word] = positional(rest, spec)
            && !shell_script(word)
        {
            out.push(cmd[start..].join(" "));
        }
        if TAKES_COMMANDS.contains(&name.as_str()) {
            for a in rest.iter().filter(|a| a.contains(char::is_whitespace)) {
                out.extend(check(&tokenize(a)));
            }
        }
    }
    out
}

/// The arguments left after tmux's getopt (`args_parse`: stops at the first
/// non-flag or `--`).
fn positional<'a>(args: &'a [String], spec: &str) -> &'a [String] {
    let mut i = 0;
    while let Some(a) = args.get(i) {
        if a == "--" {
            return &args[i + 1..];
        }
        let Some(flags) = a.strip_prefix('-').filter(|f| !f.is_empty()) else {
            break;
        };
        i += 1;
        for (j, c) in flags.char_indices() {
            if spec.contains(&format!("{c}:")) {
                if j + c.len_utf8() == flags.len() {
                    i += 1; // the value is the next argument
                }
                break;
            }
        }
    }
    &args[i.min(args.len())..]
}

/// A tmux command line split into words: whitespace separates, `'..'` and
/// `".."` group, a backslash escapes, an unquoted trailing `;` is its own word.
fn tokenize(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut quote = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"') | None, '\\') => {
                cur.extend(chars.next());
                have = true;
            }
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                have = true;
            }
            (None, ';') => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
                out.push(";".into());
            }
            (None, c) if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            (None, c) => {
                cur.push(c);
                have = true;
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
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

    #[test]
    fn one_word_and_shell_string_commands_are_flagged() {
        let sc = scenario(
            r#"
            { run = ["new", "-d", "-s", "a", "-n", "w", "cat"] },
            { run = ["splitw", "-d", "-t", "a:0", "cat -"] },
            { run = ["neww", "-t", "a:", "cat", "-"] },
            { run = ["new", "-d", "-s", "b"] },
            { run = ["respawn-pane", "-k", "-t", "%0", "cat"] },
            { run = ["splitw", "-d", "-t", "a:0", ";", "neww", "cat"] },
            { run = ["new-pane", "-E", "-x", "10"] },
            { run = ["splitw", "-d", "sh", "-c", "exec cat"] },
            { run = ["splitw", "-d", "exec cat"] },
            { run = ["neww", "-d", "printf x; exec cat"] },
            { run = ["neww", "-d", "exit 0"] },
            { control_send = { id = "c", line = "neww -t b: -n nw cat" } },
            { control = { argv = ["-f", "x.conf", "new", "-s", "c", "cat"], id = "c" } },
            { run = ["set-hook", "-g", "after-new-window", "split-window -d cat"] },
            { run = ["display-message", "-p", "new window"] }
            "#,
        );
        let bad = one_word_panes(&sc);
        let steps: Vec<_> = bad.iter().map(|b| b.split(':').next().unwrap()).collect();
        assert_eq!(
            steps,
            [
                "step 1", "step 2", "step 5", "step 6", "step 12", "step 13", "step 14"
            ],
            "{bad:#?}"
        );
    }

    #[test]
    fn flag_values_are_not_the_command() {
        assert_eq!(
            check(&argv(&["new", "-d", "-s", "a", "-x", "80", "-y", "23"])).len(),
            0
        );
        assert_eq!(
            check(&argv(&["new", "-ds", "a", "-n", "w", "cat", "-"])).len(),
            0
        );
        // `-s` takes the rest of its argument (`a`), not the next one.
        assert_eq!(check(&argv(&["new", "-dsa", "cat"])).len(), 1);
        assert_eq!(check(&argv(&["new", "-ds", "a", "--", "cat"])).len(), 1);
    }

    #[test]
    fn tokenizing() {
        assert_eq!(
            tokenize(r#"neww -n 'a b' "c\"d" e;f ; g"#),
            ["neww", "-n", "a b", "c\"d", "e", ";", "f", ";", "g"]
        );
    }

    #[test]
    fn no_scenario_starts_a_pane_with_a_one_word_command() {
        let offenders =
            all_one_word_panes(&super::super::workspace_root()).expect("scenarios load");
        assert!(
            offenders.is_empty(),
            "one-word pane commands run as `$SHELL -c word`, which shows `sh` as the pane's \
             command under dash; pass two words (`cat`, `-`) as separate arguments:\n{}",
            offenders.join("\n")
        );
    }
}
