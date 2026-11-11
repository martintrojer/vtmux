# A blank screen after an inserted upgrade, 2026-11-11

Task `flake_replay_upgrade_screen_blank`. Seen on dev1 at load 50-110: `oracle replay --upgrade-every 3` showed 6 `screen_*` scenarios blank after an inserted upgrade, and one of 5 stress batches on `069845bd` lost 5 of 15. Host here: the 96-cpu Linux server, shared, load 18-60; debug builds.

## The cause

The harness, not the product. After an upgrade the runner waited for the re-attached client's `\e[?996n` (the restart frame, `REATTACHED`), then for 150 ms of quiet. The redraw is not in that frame. The server holds a client's next frame until the client has acknowledged the one before (D14; `Server::defer_frame` while `unacked.bytes` is not 0), so the redraw goes out a round trip later: the client writes the restart frame, acknowledges it, the server renders. A host that holds the client or the server for longer than the quiet time, and again through the `screen` step's own settle, lets the runner read the screen the restart left: blank (`clear` at attach, and the alternate screen the teardown left). Several scenarios running at once lose it together under one stall, as the 5-of-15 batch did.

The redraw is never lost: with the fix's wait timed, 2173 of 2173 re-attached screens in a full `--upgrade-every 3` run showed again what they showed before, in a median 1 ms, p99 24 ms; 3 `screen_*` batches under 192 busy loops took at most 88 ms. The one that did not (`format_jobs_border`) differs by design: the new image runs its `#()` job again and shows the newer output, which the scenario's next step expects anyway.

The parked two-client report (one of two clients on a window of four agent panes shows blank rows) is not this. No upgrade is involved there and nothing is missed here. A probe with four panes redrawing under `?2026` and two clients attached matched tmux 3.8 5/5 under 48 busy loops, and 3/3 with an upgrade after every step (`live --upgrade-every 1`). Still not reproduced.

## The fix

`Runner::redrawn`: after the re-attach, the runner waits until the client shows the screen from just before the upgrade (decision 13), polling every 20 ms for up to 10 s, then settles. A screen the upgrade changes by design costs that 10 s once and is judged by the next observation, as before. Wall time of the full `--upgrade-every 3` run: 2m09s before, 2m10s after.

## Tests

| Check | Result |
|---|---|
| `oracle::runner::tests::the_screen_after_an_upgrade_waits_for_the_redraw` (the server stopped for 1 s right after it answers the upgrade) | fails 10/10 before (the screen after is 5 blank rows), passes after |
| Same test plus `replay --upgrade-every 3 --only 'screen_*,format_jobs_*,lnm_respawn_upgrade'`, 5x under 48 busy loops (`flock /tmp/vtmux-stress.lock`) | 5/5, 18/18 each |
| Same, once with a dash-like `/bin/sh` (bash `--posix` that forks its last command, bind-mounted in a user and mount namespace) | pass, 18/18 |
| Full `replay --upgrade-every 3`, `--upgrade-every 1 --only …` (`check.sh`'s list) | 438/438, 9/9 |

Before the fix, 17 batches of `screen_*` with `--upgrade-every 3` (12 under 48 busy loops, 5 under 192) and a full run under 48 did not fail here: this host's scheduler does not stall a woken process for 300 ms at that load, so the stopped server stands in for dev1's stall.
