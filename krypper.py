import argparse
from collections import deque
import json
import os
import stat
import sys


MAX_FILE_SIZE = 1024 * 1024
MAX_SYMLINKS = 40


def open_cwd_relative(path, *, directory=False):
    if os.path.isabs(path):
        raise ValueError("absolute paths are not allowed")

    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    root_fd = os.open(".", directory_flags)
    descriptors = [root_fd]
    pending = deque(path.split(os.sep))
    root_path = os.path.realpath(".")
    symlinks = 0

    try:
        while pending:
            component = pending.popleft()
            if component in ("", "."):
                continue
            if component == "..":
                if len(descriptors) == 1:
                    raise ValueError("path must stay inside the current directory")
                os.close(descriptors.pop())
                continue

            parent_fd = descriptors[-1]
            try:
                info = os.stat(component, dir_fd=parent_fd, follow_symlinks=False)
            except FileNotFoundError:
                raise ValueError("file not found") from None
            except OSError:
                raise ValueError("invalid path") from None

            if stat.S_ISLNK(info.st_mode):
                symlinks += 1
                if symlinks > MAX_SYMLINKS:
                    raise ValueError("invalid path")
                try:
                    target = os.readlink(component, dir_fd=parent_fd)
                except OSError:
                    raise ValueError("invalid path") from None
                if os.path.isabs(target):
                    target = os.path.normpath(target)
                    try:
                        inside = os.path.commonpath((root_path, target)) == root_path
                    except ValueError:
                        inside = False
                    if not inside:
                        raise ValueError("path must stay inside the current directory")
                    root_info = os.fstat(root_fd)
                    try:
                        named_root = os.stat(root_path)
                    except OSError:
                        raise ValueError("invalid path") from None
                    if (root_info.st_dev, root_info.st_ino) != (named_root.st_dev, named_root.st_ino):
                        raise ValueError("invalid path")
                    while len(descriptors) > 1:
                        os.close(descriptors.pop())
                    target = os.path.relpath(target, root_path)
                pending.extendleft(reversed(target.split(os.sep)))
                continue

            final = not any(part not in ("", ".") for part in pending)
            if final and pending and not directory:
                final = False
            if final and not (stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)):
                raise ValueError("not a directory" if directory else "not a regular file")
            flags = os.O_RDONLY | os.O_NOFOLLOW | getattr(os, "O_NONBLOCK", 0)
            if not final or directory:
                flags |= os.O_DIRECTORY
            try:
                descriptor = os.open(component, flags, dir_fd=parent_fd)
            except FileNotFoundError:
                raise ValueError("file not found") from None
            except OSError:
                raise ValueError("unable to read file") from None

            if final:
                return descriptor
            descriptors.append(descriptor)

        return os.dup(root_fd)
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)


def read_text_file(path):
    fd = open_cwd_relative(path)
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


def list_directory(path):
    fd = open_cwd_relative(path, directory=True)
    try:
        return sorted(os.listdir(fd))
    except OSError:
        raise ValueError("unable to list directory") from None
    finally:
        os.close(fd)


def create_text_file(path, text):
    if "\0" in path:
        raise ValueError("invalid file path")
    if os.path.isabs(path):
        raise ValueError("absolute paths are not allowed")
    if not path or path.endswith(os.sep):
        raise ValueError("invalid file path")

    try:
        contents = text.encode("utf-8")
    except UnicodeEncodeError:
        raise ValueError("text is not valid UTF-8") from None
    if len(contents) > MAX_FILE_SIZE:
        raise ValueError("text exceeds 1 MiB")

    components = path.split(os.sep)
    name = components[-1]
    if name in ("", ".", ".."):
        raise ValueError("invalid file path")

    directory_flags = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW
    try:
        root_fd = os.open(".", directory_flags)
    except OSError:
        raise ValueError("unable to open current directory") from None
    directories = [root_fd]
    try:
        for component in components[:-1]:
            if component in ("", "."):
                continue
            if component == "..":
                if len(directories) == 1:
                    raise ValueError("path must stay inside the current directory")
                os.close(directories.pop())
                continue

            parent_fd = directories[-1]
            try:
                info = os.stat(component, dir_fd=parent_fd, follow_symlinks=False)
            except FileNotFoundError:
                raise ValueError("parent directory not found") from None
            except OSError:
                raise ValueError("invalid parent directory") from None
            if stat.S_ISLNK(info.st_mode):
                raise ValueError("symlink paths are not allowed")
            if not stat.S_ISDIR(info.st_mode):
                raise ValueError("parent is not a directory")

            try:
                descriptor = os.open(component, directory_flags, dir_fd=parent_fd)
            except OSError:
                raise ValueError("invalid parent directory") from None
            opened = os.fstat(descriptor)
            if (info.st_dev, info.st_ino) != (opened.st_dev, opened.st_ino):
                os.close(descriptor)
                raise ValueError("path changed while creating file")
            directories.append(descriptor)

        parent_fd = directories[-1]
        try:
            file_fd = os.open(
                name,
                os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                0o666,
                dir_fd=parent_fd,
            )
        except FileExistsError:
            raise ValueError("file already exists") from None
        except FileNotFoundError:
            raise ValueError("parent directory not found") from None
        except OSError:
            raise ValueError("unable to create file") from None

        created = None
        write_failed = False
        try:
            created = os.fstat(file_fd)
            offset = 0
            while offset < len(contents):
                try:
                    written = os.write(file_fd, contents[offset:])
                except InterruptedError:
                    continue
                if written <= 0:
                    raise OSError("file write made no progress")
                offset += written
        except OSError:
            write_failed = True
            if created is None:
                try:
                    created = os.fstat(file_fd)
                except OSError:
                    pass
        try:
            os.close(file_fd)
        except OSError:
            write_failed = True

        if write_failed:
            try:
                if created is not None:
                    current = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
                    if (current.st_dev, current.st_ino) == (created.st_dev, created.st_ino):
                        os.unlink(name, dir_fd=parent_fd)
            except OSError:
                pass
            raise ValueError("unable to write file") from None
    finally:
        for descriptor in reversed(directories):
            os.close(descriptor)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="krypper",
        description="Local command-line entry point for Krypper.",
    )
    commands = parser.add_subparsers(dest="command")
    show = commands.add_parser("show", help="display a UTF-8 text file")
    show.add_argument("path", help="relative path inside the current directory")
    listing = commands.add_parser("list", help="list immediate directory entries")
    listing.add_argument("path", nargs="?", default=".", help="relative directory inside the current directory")
    writer = commands.add_parser("write", help="create a new UTF-8 text file")
    writer.add_argument("path", help="relative file path inside the current directory")
    writer.add_argument("text", nargs=argparse.REMAINDER, help="text to write")
    args = parser.parse_args(argv)

    if args.command is None:
        parser.error("a command is required")
    elif args.command == "show":
        try:
            sys.stdout.buffer.write(read_text_file(args.path).encode("utf-8"))
        except ValueError as error:
            show.error(str(error))
    elif args.command == "list":
        try:
            for name in list_directory(args.path):
                print(json.dumps(name, ensure_ascii=True))
        except ValueError as error:
            listing.error(str(error))
    elif args.command == "write":
        if not args.text:
            writer.error("text is required")
        try:
            create_text_file(args.path, " ".join(args.text))
        except ValueError as error:
            writer.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
