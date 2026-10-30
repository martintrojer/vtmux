//! `kill-server` (cmd-kill-server.c): the server exits once this batch is
//! answered (tmux sends itself SIGTERM, server.c `server_send_exit`).

use crate::server::Server;

pub fn exec(server: &mut Server) {
    server.shutdown = true;
}
