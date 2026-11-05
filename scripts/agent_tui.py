#!/usr/bin/env python3
"""A synthetic agent TUI for scripts/bench.py's agent_panes scenario.

Mimics what pi (pi-tui's alt-screen renderer) sends while an agent works, as recorded
from a real `pi` run in a 100x30 PTY (about 30 frames and 13 KB a second):
- every frame is wrapped in synchronized output (ESC[?2026h ... ESC[?2026l) and
  rewrites only the rows that changed, each as `ESC[r;1H ESC[2K <line> ESC[0m OSC 8;; BEL`,
  then hides the cursor;
- a spinner row (braille frame, truecolour) every 80 ms;
- streaming text: a word every 20-80 ms appended to the last chat row; when the row
  wraps and the chat area is full, the whole chat area is redrawn (pi re-renders its
  scroll view); now and then a block of tool output lands at once;
- a footer with a token count once a second.

Deterministic for a seed (the timing follows the wall clock, the content does not).
Runs until killed. Usage: agent_tui.py SEED  (the pane size comes from the tty).
"""
import fcntl
import os
import random
import signal
import struct
import sys
import termios
import time

SPIN = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
WORDS = ("the agent reads the file and checks each call site before it edits the parser so "
         "that the render cache keeps rows which did not change between two frames while a "
         "client is attached to the server and the pane writes synchronized output").split()
DIM, TEXT, ACCENT, OK = "\x1b[38;2;108;118;123m", "\x1b[38;2;205;214;220m", "\x1b[38;2;79;142;179m", "\x1b[38;2;134;180;111m"
ROW_END = "\x1b[0m\x1b]8;;\x07"


def size():
    try:
        rows, cols = struct.unpack("HHHH", fcntl.ioctl(1, termios.TIOCGWINSZ, b"\0" * 8))[:2]
        if rows and cols:
            return cols, rows
    except OSError:
        pass
    return 100, 30


def main():
    rnd = random.Random(int(sys.argv[1]) if len(sys.argv) > 1 else 1)
    cols, rows = size()
    chat_h = max(1, rows - 5)  # chat rows 1..chat_h; then blank, spinner, editor border, editor, footer
    width = max(10, cols - 2)
    out = sys.stdout.buffer

    lines = [""]  # wrapped chat lines (plain text); the last one is being streamed
    tokens = 0
    spin = 0

    def pad(s):
        return s + " " * max(0, cols - 1 - len(s))

    def chat_row(i):
        top = max(0, len(lines) - chat_h)
        j = top + i
        text = lines[j] if j < len(lines) else ""
        colour = OK if text.startswith("  $") else TEXT
        return f" {colour}{pad(text)}"

    def spinner_row():
        label = f" {SPIN[spin % len(SPIN)]} Working "
        return f"{DIM}──{label}{'─' * max(0, cols - 4 - len(label))}\x1b[39m"

    def footer_row():
        return f"{DIM}~/src/project (main) {ACCENT}↑{tokens // 7} ↓{tokens}{DIM} 23.4%/200k\x1b[39m"

    def frame(changed):
        buf = ["\x1b[?2026h"]
        for r, text in changed:
            buf.append(f"\x1b[{r};1H\x1b[2K{text}{ROW_END}")
        buf.append(f"\x1b[{rows - 1};3H\x1b[?25l\x1b[?2026l")
        out.write("".join(buf).encode())
        out.flush()

    def full():
        changed = [(i + 1, chat_row(i)) for i in range(chat_h)]
        changed += [(chat_h + 1, ""), (chat_h + 2, spinner_row()), (chat_h + 3, f"{DIM}{'─' * (cols - 1)}\x1b[39m"),
                    (chat_h + 4, f"{ACCENT}> {TEXT}"), (chat_h + 5, footer_row())]
        out.write(b"\x1b[?1049h\x1b[?7l\x1b[2J\x1b[H\x1b[?25l")
        frame(changed)

    def append(text):
        """Add text to the streamed line; returns the chat rows to redraw."""
        before = len(lines)
        for word in text.split(" "):
            if len(lines[-1]) + 1 + len(word) > width:
                lines.append(word)
            else:
                lines[-1] = f"{lines[-1]} {word}" if lines[-1] else word
        del lines[:-1000]
        if len(lines) == before:
            return [min(len(lines), chat_h)]
        if len(lines) > chat_h:
            return list(range(1, chat_h + 1))  # scrolled: the whole chat area
        return list(range(before, len(lines) + 1))

    resized = []
    signal.signal(signal.SIGWINCH, lambda *_: resized.append(1))
    full()
    now = time.monotonic()
    next_spin, next_word, next_footer, next_block = now + 0.08, now + 0.05, now + 1.0, now + rnd.uniform(2, 5)
    while True:
        t = min(next_spin, next_word, next_footer, next_block)
        delay = t - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        now = time.monotonic()
        if resized:  # pi redraws everything on a resize
            resized.clear()
            cols, rows = size()
            chat_h, width = max(1, rows - 5), max(10, cols - 2)
            full()
        changed = []
        if now >= next_word:
            changed += append(rnd.choice(WORDS))
            tokens += 1
            next_word = now + rnd.uniform(0.02, 0.08)
        if now >= next_block:
            lines.append("")
            for _ in range(rnd.randrange(4, 12)):
                lines.append("  $ " + " ".join(rnd.choice(WORDS) for _ in range(rnd.randrange(3, 12)))[:width - 4])
            lines.append("")
            changed += list(range(1, chat_h + 1))
            next_block = now + rnd.uniform(2, 5)
        changed = [(r, chat_row(r - 1)) for r in sorted(set(changed))]
        if now >= next_spin:
            spin += 1
            changed.append((chat_h + 2, spinner_row()))
            next_spin += 0.08
            if next_spin < now:
                next_spin = now + 0.08
        if now >= next_footer:
            changed.append((chat_h + 5, footer_row()))
            next_footer = now + 1.0
        if changed:
            frame(changed)


if __name__ == "__main__":
    try:
        main()
    except (BrokenPipeError, KeyboardInterrupt):
        pass
