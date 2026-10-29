"""Stream one bounded regular file through pinned directory descriptors."""

import os
import stat
import sys


def main():
    if len(sys.argv) == 2 and sys.argv[1] == "--probe":
        if not {os.open, os.stat}.issubset(os.supports_dir_fd):
            raise RuntimeError("Python3 lacks descriptor-relative filesystem support")
        for name in ("O_NOFOLLOW", "O_DIRECTORY", "O_NONBLOCK", "O_CLOEXEC"):
            getattr(os, name)
        return
    root, path, maximum = sys.argv[1:]
    parts = path.split("/")
    if not parts or any(part in ("", ".", "..") for part in parts):
        raise ValueError("capture path must be relative without traversal")
    flags = os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC
    directory = os.open(root, flags | os.O_DIRECTORY)
    try:
        for part in parts[:-1]:
            child = os.open(part, flags | os.O_DIRECTORY, dir_fd=directory)
            os.close(directory)
            directory = child
        source = os.open(parts[-1], flags | os.O_NONBLOCK, dir_fd=directory)
    finally:
        os.close(directory)
    with os.fdopen(source, "rb", buffering=0) as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("captured input must be a regular file")
        remaining = int(maximum)
        while True:
            chunk = stream.read(min(65536, remaining + 1))
            if not chunk:
                break
            if len(chunk) > remaining:
                raise ValueError("captured input exceeds its byte limit")
            sys.stdout.buffer.write(chunk)
            remaining -= len(chunk)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, RuntimeError) as error:
        print("artifact capture: " + str(error), file=sys.stderr)
        sys.exit(1)
