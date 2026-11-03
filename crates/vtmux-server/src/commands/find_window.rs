//! `find-window` (cmd-find-window.c:44-117): tree mode on the target pane
//! with a filter built from the match string. `-C`, `-N` and `-T` search
//! pane content (`#{C:}`), window names and pane titles; none of them is
//! all three. `-r` makes the string a regex, `-i` ignores case, `-Z`
//! zooms the pane.

use vtmux_core::cmd::{Args, CmdError};

use super::Item;
use crate::mode::window_tree::{Options, Start};
use crate::server::Server;
use crate::target::Found;

pub fn exec(
    server: &mut Server,
    _item: &mut Item,
    args: &Args,
    found: Found,
) -> Result<(), CmdError> {
    let options = Options {
        start: Start::Pane,
        previews: 0,
        sort: None,
        reversed: false,
        filter: Some(filter(args)),
        format: None,
        key_format: None,
        command: None,
        hide_this_pane: false,
        accept: false,
    };
    super::choose_tree::open(server, found, options, args.flag('Z'), false)
}

/// The `choose-tree -f` filter `cmd_find_window_exec` builds
/// (cmd-find-window.c:59-106).
fn filter(args: &Args) -> String {
    let s = args.positional_text(0).unwrap_or_default();
    let (c, n, t) = (args.flag('C'), args.flag('N'), args.flag('T'));
    let (c, n, t) = if c || n || t {
        (c, n, t)
    } else {
        (true, true, true)
    };
    let regex = args.flag('r');
    let star = if regex { "" } else { "*" };
    let suffix = match (regex, args.flag('i')) {
        (true, true) => "/ri",
        (true, false) => "/r",
        (false, true) => "/i",
        (false, false) => "",
    };
    let content = format!("#{{C{suffix}:{s}}}");
    let name = format!("#{{m{suffix}:{star}{s}{star},#{{window_name}}}}");
    let title = format!("#{{m{suffix}:{star}{s}{star},#{{pane_title}}}}");
    match (c, n, t) {
        (true, true, true) => format!("#{{||:{content},#{{||:{name},{title}}}}}"),
        (true, true, false) => format!("#{{||:{content},{name}}}"),
        (true, false, true) => format!("#{{||:{content},{title}}}"),
        (false, true, true) => format!("#{{||:{name},{title}}}"),
        (true, false, false) => content,
        (false, true, false) => name,
        _ => title,
    }
}

#[cfg(test)]
mod tests {
    use vtmux_core::cmd::{Args, table};

    fn filter(words: &[&str]) -> String {
        let words: Vec<String> = std::iter::once("find-window")
            .chain(words.iter().copied())
            .map(str::to_owned)
            .collect();
        let args = Args::parse(table::find("find-window").unwrap(), &words).unwrap();
        super::filter(&args)
    }

    #[test]
    fn filters_match_cmd_find_window() {
        assert_eq!(
            filter(&["x"]),
            "#{||:#{C:x},#{||:#{m:*x*,#{window_name}},#{m:*x*,#{pane_title}}}}"
        );
        assert_eq!(
            filter(&["-CNri", "x"]),
            "#{||:#{C/ri:x},#{m/ri:x,#{window_name}}}"
        );
        assert_eq!(
            filter(&["-CTi", "x"]),
            "#{||:#{C/i:x},#{m/i:*x*,#{pane_title}}}"
        );
        assert_eq!(
            filter(&["-NTr", "x"]),
            "#{||:#{m/r:x,#{window_name}},#{m/r:x,#{pane_title}}}"
        );
        assert_eq!(filter(&["-C", "x"]), "#{C:x}");
        assert_eq!(filter(&["-N", "x"]), "#{m:*x*,#{window_name}}");
        assert_eq!(filter(&["-T", "x"]), "#{m:*x*,#{pane_title}}");
    }
}
