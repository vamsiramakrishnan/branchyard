"""A real Unix PTY with bounded JSON control frames over provider stdio."""

import base64
import contextlib
import errno
import fcntl
import json
import os
import pty
import selectors
import signal
import struct
import sys
import termios

MAX_INPUT = 32768


def emit(value):
    """Write one library-encoded protocol frame, never raw terminal output."""
    sys.stdout.write(json.dumps(value, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def dimensions(rows, columns):
    """Validate terminal dimensions before passing them to ioctl."""
    if type(rows) is not int or type(columns) is not int or not 1 <= rows <= 1000 or not 1 <= columns <= 1000:
        raise ValueError("terminal dimensions must be integers between 1 and 1000")
    return struct.pack("HHHH", rows, columns, 0, 0)


def main():
    """Run the fixed operator shell and tear its process group down on close."""
    shell, rows, columns = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    initial = dimensions(rows, columns)
    pid, master = pty.fork()
    if pid == 0:
        args = [shell, "-i"]
        if os.path.basename(shell) == "bash":
            args = [shell, "--noprofile", "--norc", "-i"]
        os.execvpe(shell, args, os.environ)
    fcntl.ioctl(master, termios.TIOCSWINSZ, initial)
    os.set_blocking(master, False)
    selector = selectors.DefaultSelector()
    selector.register(master, selectors.EVENT_READ)
    selector.register(sys.stdin.fileno(), selectors.EVENT_READ)
    pending = bytearray()
    writing = bytearray()
    try:
        emit({"kind": "ready", "pid": pid})
        while True:
            for key, events in selector.select():
                if key.fd == master:
                    if events & selectors.EVENT_READ:
                        try:
                            output = os.read(master, 16384)
                        except OSError as error:
                            if error.errno == errno.EIO:
                                return
                            raise
                        if not output:
                            return
                        emit({"kind": "output", "data": base64.b64encode(output).decode("ascii")})
                    if events & selectors.EVENT_WRITE and writing:
                        with contextlib.suppress(BlockingIOError):
                            del writing[: os.write(master, writing[:4096])]
                        selector.modify(master, selectors.EVENT_READ | (selectors.EVENT_WRITE if writing else 0))
                else:
                    chunk = os.read(sys.stdin.fileno(), 16384)
                    if not chunk:
                        return
                    pending.extend(chunk)
                    if len(pending) > MAX_INPUT:
                        raise ValueError("terminal input frame exceeds its byte limit")
                    while b"\n" in pending:
                        line, _, rest = pending.partition(b"\n")
                        pending = bytearray(rest)
                        command = json.loads(line)
                        if set(command) == {"input"}:
                            data = base64.b64decode(command["input"], validate=True)
                            if len(data) > 16384:
                                raise ValueError("terminal input exceeds its byte limit")
                            if len(writing) + len(data) > MAX_INPUT:
                                raise ValueError("terminal pending input exceeds its byte limit")
                            writing.extend(data)
                            selector.modify(master, selectors.EVENT_READ | selectors.EVENT_WRITE)
                        elif set(command) == {"resize"}:
                            size = dimensions(*command["resize"])
                            fcntl.ioctl(master, termios.TIOCSWINSZ, size)
                        elif command == {"close": True}:
                            return
                        else:
                            raise ValueError("unknown terminal command")
    finally:
        selector.close()
        with contextlib.suppress(ProcessLookupError):
            os.killpg(pid, signal.SIGKILL)
        os.close(master)
        os.waitpid(pid, 0)
        emit({"kind": "closed"})


if __name__ == "__main__":
    main()
