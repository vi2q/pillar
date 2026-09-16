#!/usr/bin/env python3
"""Run a TUI in a real pty and print the escape bytes it wrote.

Why: render and exit behaviour depends on real terminal timing (the render
throttle, the input cadence) in ways an in-process fake terminal does not
reproduce. This is the ground-truth probe used for docs/RENDER-PARITY-*.md:
it opens a pty, feeds scripted input after given delays, and dumps the tail of
the raw output.

Usage:
    python3 scripts/pty_probe.py '["pi"]' '[[3.0,"/quit\\r"]]' 12

Arguments:
    1. the command, as a JSON array
    2. writes, as JSON [[seconds, payload], ...] (`\\r` = Enter, `\\u001b` = Escape)
    3. how many seconds to watch (optional, default 12)

The dump is the repr of the last 4000 bytes. Frames are delimited by
`\\x1b[?2026h` / `\\x1b[?2026l` (synchronized output), so
`out.split(b"\\x1b[?2026h")[-1]` is the final frame.
"""

import contextlib
import fcntl
import json
import os
import pty
import select
import signal
import struct
import sys
import termios
import time


def run(cmd, writes, size=(24, 80), total=12.0):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        # The whole point of this probe is to exec the command under test.
        os.execvp(cmd[0], cmd)  # noqa: S606
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", size[0], size[1], 0, 0))
    start = time.time()
    out = b""
    plan = list(writes)
    while time.time() - start < total:
        readable, _, _ = select.select([fd], [], [], 0.1)
        if readable:
            try:
                data = os.read(fd, 65536)
            except OSError:
                break
            if not data:
                break
            out += data
        while plan and time.time() - start >= plan[0][0]:
            _, payload = plan.pop(0)
            os.write(fd, payload)
        try:
            wpid, _ = os.waitpid(pid, os.WNOHANG)
        except ChildProcessError:
            break
        if wpid:
            break
    with contextlib.suppress(ProcessLookupError):
        os.kill(pid, signal.SIGKILL)
    with contextlib.suppress(OSError):
        os.close(fd)
    return out


if __name__ == "__main__":
    command = json.loads(sys.argv[1])
    scripted = [
        (float(at), payload.encode().decode("unicode_escape").encode())
        for at, payload in json.loads(sys.argv[2])
    ]
    seconds = float(sys.argv[3]) if len(sys.argv) > 3 else 12.0
    sys.stdout.write(repr(run(command, scripted, total=seconds)[-4000:]))
