# Pane blend: make the active pane obvious without spending a cell

## Problem

The owner works in a two-pane vertical split almost all the time and keeps typing into the wrong pane. tmux marks the active pane with a half-coloured shared divider and one arrow cell (`pane-border-indicators`). That's a clue you have to decode, not something peripheral vision picks up. The focus ring (D23, `vtmux-focus-ring`) fixes it with a closed heavy rectangle, but it costs a 1-cell frame around the window, and for this the owner wants no space cost.

The cue the owner already trusts is Ghostty's `unfocused-split-opacity`: every pixel of an unfocused split is drawn 25% toward the background colour (Ghostty `Config.zig`, `unfocused-split-opacity` / `unfocused-split-fill`; it draws a translucent rectangle over the split). That has never worked well inside tmux. tmux's `window-style` / `window-active-style` only reach cells that use default colours, so a TUI that paints its own colours (pi, nvim, htop) barely changes.

vtmux composes every client frame itself (D14), so it can change the colour of every cell of a pane before encoding. tmux can't do this.

## Approach

Two window options, both vtmux-only, empty (off) by default:

- `vtmux-pane-active-blend`: applied to the active pane of the window a client views.
- `vtmux-pane-inactive-blend`: applied to every other pane of that window.

The value is style-like: `fg=<colour>[/<pct>],bg=<colour>[/<pct>]`, either part optional, in any order.

- `<colour>` is anything tmux accepts in a style (`#rrggbb`, `colourN`, names), plus two keywords: `bg` and `fg` mean the outer terminal's default background and foreground.
- `/<pct>` (0-100) moves each cell's colour that far toward `<colour>` by linear interpolation in sRGB, per channel. `/100` (the default when `/pct` is left out) replaces the colour.
- The cell's foreground (glyph colour, including default-fg cells) blends toward the `fg=` target and its background (including default-bg cells) toward the `bg=` target. Underline colour follows fg. Reverse video: blend after swapping, so what's on screen is what gets blended.

The owner's configs:

```tmux
%if "#{vtmux_version}"
set -g vtmux-pane-inactive-blend 'fg=bg/25,bg=bg/25'        # Ghostty-like dim
# or: dim only text, darker base, slight lift on the active pane
# set -g vtmux-pane-inactive-blend 'fg=bg/35,bg=#181825'
# set -g vtmux-pane-active-blend 'bg=#24273a'
%endif
```

## Decisions

| Decision | Choice | Why |
|---|---|---|
| Option shape | Two style-like options, not one option per colour and percent | Everything about one pane state fits on one line, it reads like tmux styles, and one line gets Ghostty's look (owner, Q6) |
| Separate fg and bg | Yes | fg alone dims text; bg alone tints the base; both together match Ghostty (owner) |
| Default | Off (empty) | Paper-cut rules: opt-in, invisible to scripts while unset (vtmux option table, PAPERCUTS *How vtmux options stay invisible*) |
| `bg`/`fg` keywords | The outer terminal's default colours, from its OSC 10/11 replies. tmux 3.8 ALWAYS sends `ESC]10;?ST ESC]11;?ST` in `tty_send_requests` (tty.c:420) and stores the replies (tty->fg/bg, tty_keys_colours); vtmux's `Caps::startup` sends them since gap_term_queries, which keeps the replies per client (`term_queries::Reported`); the blend reads that store, adding OSC 4 0-15 to it (D-033). Re-query on a theme change (mode 2031 / DSR 996 notification), as tmux's `tty_repeat_requests` does. Until a reply arrives, or if none ever does, both blend options act as OFF for that client (no fallback colours: a light terminal must never be darkened by a black fallback); the client recomposes when the reply lands | Makes `fg=bg/25` mean "dim toward my background" in any theme (orchestrator's call: Ghostty reports Catppuccin's `#1e1e2e`) |
| Scope | Pane content only: every cell of the pane, explicit colours included, in every mode (copy, view, clock, tree), floating panes included | An inactive pane in a mode is still inactive (owner agreed) |
| Not blended | Borders, the focus ring, the status line, pane border status, popups, menus, prompts and messages | Border colours stay as a second cue, and status pills keep their meaning (owner agreed) |
| With window-style | window-style/window-active-style apply first (as tmux), then the blend | Keeps tmux semantics intact; the blend is an extra layer |
| Which pane is active | Per client: the active pane of the window that client is viewing | Two clients on different windows each see their own (owner agreed) |
| Source colour of palette cells | Indexed cells (SGR 30-37/90-97, 38;5;N) blend from the outer terminal's ACTUAL palette when known, else xterm's standard palette. This unit queries OSC 4 for indices 0-15 at attach (16 queries; the 16 base colours are what themes like Catppuccin change) and uses xterm's values for 16-255 (themes rarely change those). Cells with default fg/bg blend from the OSC 10/11 colours | The owner's TUIs paint with palette indices, and Ghostty with Catppuccin has a custom 0-15, so xterm defaults would give wrong dimmed hues (orchestrator's call) |
| Colour depth | Truecolor terminals get the exact blend. 256 colours: the nearest palette entry (the palette's RGB values from xterm's standard, or the terminal's OSC 4 replies when known). 16 colours or fewer: inactive panes fall back to SGR 2 (faint), and no bg change | It's the only cue that works there (owner agreed) |
| Animations | None. The change is instant on focus change | Animations are a non-goal |
| Zoomed window | One visible pane, so it counts as active: no inactive blend | Nothing to tell apart |
| Cost | The blend runs only in compose for rows that changed, as part of the row cache key (pane active flag, option value, terminal default colours). Focus changes recompose the two affected panes' rows | Must not undo key_echo_latency's caches or the frame skip |

## Scope

In: the two options and their parsing and validation (`set` rejects a bad value with tmux's style-error wording); blending in the compositor; the `bg`/`fg` keywords from OSC 10/11; colour-depth fallbacks; docs (PAPERCUTS row + *vtmux options* reference; this spec); tests.

Out: per-pane overrides (window options only, which still allows per-window settings), animation, blending borders or status, a gradient across a pane.

## Tests

- Unit: blend math (0, 25, 100 percent; default fg/bg; reverse video; underline colour), and option parsing (both parts, either part, bad colour, pct out of range).
- Attach tests, against a client with a known OSC 11 reply: an inactive pane's explicitly coloured cells come out blended and the active pane's don't; select-pane swaps them in one frame with no stale rows; the options unset give byte-identical output to before (off means off); a 256-colour client gets palette indices; a 16-colour client gets SGR 2.
- The frame-skip, sync-hold and row-cache tests stay green.
- Off state: apart from the new startup queries (which match tmux), output with the options unset is unchanged; `oracle replay` passes.
- No reply: a client whose terminal never answers OSC 10/11 gets no blend (byte-identical to off).
- Perf: the agent_panes and render benches with the blend on stay within 10% of off.

## Implementation checklist

1. `crates/vtmux-core/src/options/vtmux.rs`: two window options, a style-like parser for `fg=`/`bg=` with `/pct` and the keywords.
2. Terminal colours: send `OSC 10;?`, `OSC 11;?` (always, as tmux tty.c:420) and `OSC 4;0..15;?` at attach; parse the replies (input.rs recognises the prefixes, today it ignores them) into a per-client store; re-query on a theme change. The oracle/attach traces of the startup bytes will change: update the expected traces (the reference tmux sends 10/11 too). Coordinate with gap_term_queries (another worker answering a PANE's colour queries from the client's colours): rebase on it if it lands first and share the store.
3. Compose: apply the blend per cell for a pane, by pane role (active or inactive), after window-style; put it in the row cache key.
4. Encoder: colour-depth fallbacks.
5. Docs: PAPERCUTS row, *vtmux options* section; README pointer if the README lists vtmux options.
6. Tests per above.
