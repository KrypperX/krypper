import argparse
import os
import stat
import sys
from pathlib import Path


MAX_FILE_SIZE = 1024 * 1024


def read_text_file(path):
    current_dir = Path.cwd().resolve()
    selected = Path(path)
    if selected.is_absolute():
        raise ValueError("absolute paths are not allowed")

    try:
        resolved = selected.resolve(strict=True)
    except FileNotFoundError:
        raise ValueError("file not found") from None
    except (OSError, RuntimeError, ValueError):
        raise ValueError("invalid path") from None

    try:
        resolved.relative_to(current_dir)
    except ValueError:
        raise ValueError("path must stay inside the current directory") from None

    try:
        fd = os.open(resolved, os.O_RDONLY | getattr(os, "O_NONBLOCK", 0))
    except FileNotFoundError:
        raise ValueError("file not found") from None
    except OSError:
        raise ValueError("unable to read file") from None

    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("not a regular file")
        if info.st_size > MAX_FILE_SIZE:
            raise ValueError("file exceeds 1 MiB")
        with os.fdopen(fd, "rb") as file:
            fd = None
            contents = file.read(MAX_FILE_SIZE + 1)
    except OSError:
        raise ValueError("unable to read file") from None
    finally:
        if fd is not None:
            os.close(fd)

    if len(contents) > MAX_FILE_SIZE:
        raise ValueError("file exceeds 1 MiB")
    try:
        return contents.decode("utf-8")
    except UnicodeDecodeError:
        raise ValueError("file is not valid UTF-8") from None


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="krypper",
        description="Local command-line entry point for Krypper.",
    )
    commands = parser.add_subparsers(dest="command")
    show = commands.add_parser("show", help="display a UTF-8 text file")
    show.add_argument("path", help="relative path inside the current directory")
    args = parser.parse_args(argv)

    if args.command == "show":
        try:
            sys.stdout.buffer.write(read_text_file(args.path).encode("utf-8"))
        except ValueError as error:
            show.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
