#!/usr/bin/env python3
"""Atomic per-file installation with rollback and recovery of interrupted transactions.

Configuration and OS credentials are outside the managed file list.
"""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import uuid


FILES = {
    "bin/rustterm": ("bin/rustterm", 0o755),
    "share/applications/io.rustterm.RustTerm.desktop": (
        "share/applications/io.rustterm.RustTerm.desktop", 0o644),
    "share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg": (
        "share/icons/hicolor/scalable/apps/io.rustterm.RustTerm.svg", 0o644),
    "share/doc/rustterm/LICENSE": ("share/doc/rustterm/LICENSE", 0o644),
    "share/doc/rustterm/README.md": ("share/doc/rustterm/README.md", 0o644),
    "share/doc/rustterm/alacritty_terminal-LICENSE-APACHE": ("share/doc/rustterm/alacritty_terminal-LICENSE-APACHE", 0o644),
    "share/doc/rustterm/build-info.json": ("build-info.json", 0o644),
}
# Old installers included the local patch notice in their transaction journals.
# Accept that fixed path for recovery, but do not require it in new packages or
# remove a notice left by an older completed installation.
RECOVERABLE_FILES = frozenset(FILES) | {"share/doc/rustterm/THIRD_PARTY_PATCHES.md"}
JOURNAL = ".rustterm-install-transaction.json"


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def regular_file(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode):
        raise ValueError(f"Expected a regular file, not a symlink: {path}")
    return info


def safe_directory(path, *, create=True):
    if create:
        path.mkdir(mode=0o755, parents=True, exist_ok=True)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
        raise ValueError(f"Installation directory must be owned by this user and not writable by others: {path}")


def destination(prefix, relative):
    path = prefix / relative
    # Reject redirected application subdirectories before modifying anything through them.
    directory = prefix
    for part in Path(relative).parts[:-1]:
        directory /= part
        safe_directory(directory)
    if path.exists() or path.is_symlink():
        info = regular_file(path)
        if info.st_uid != os.geteuid():
            raise ValueError(f"Installed file is owned by another user: {path}")
    return path


def desktop_text(source, executable):
    # Exec quoting is distinct from the Desktop Entry string-value escaping layer.
    escaped = "".join("\\" + c if c in '\\"`$' else c for c in str(executable).replace("%", "%%"))
    escaped = escaped.replace("\\", "\\\\")
    if source.count("Exec=rustterm\n") != 1:
        raise ValueError("Package desktop entry must contain exactly one Exec=rustterm line")
    return source.replace("Exec=rustterm\n", 'Exec="' + escaped + '"\n')


def write_journal(prefix, journal):
    descriptor, temporary = tempfile.mkstemp(prefix=".rustterm-journal-", dir=prefix)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as output:
            json.dump(journal, output)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, prefix / JOURNAL)
        sync_directory(prefix)
    finally:
        Path(temporary).unlink(missing_ok=True)


def transaction_paths(prefix, item, transaction_id):
    relative = item["relative"]
    if relative not in RECOVERABLE_FILES:
        raise ValueError("Invalid installation recovery record")
    target = destination(prefix, relative)
    # Recovery paths are generated from fixed file names, never taken verbatim from JSON.
    suffix = f".rustterm-{transaction_id}-{target.name}"
    return target, target.with_name(suffix + ".new"), target.with_name(suffix + ".old")


def recover(prefix):
    path = prefix / JOURNAL
    if not path.exists() and not path.is_symlink():
        return
    regular_file(path)
    if path.stat().st_size > 16_384:
        raise ValueError("Installation recovery record is too large")
    journal = json.loads(path.read_text(encoding="utf-8"))
    transaction_id = str(uuid.UUID(journal["id"]))
    if journal["state"] not in {"prepared", "committed"}:
        raise ValueError("Invalid installation recovery state")
    if len(journal["files"]) > len(RECOVERABLE_FILES):
        raise ValueError("Invalid installation recovery file count")
    for item in reversed(journal["files"]):
        target, staged, backup = transaction_paths(prefix, item, transaction_id)
        if journal["state"] == "prepared":
            if item["existed"]:
                # Keep backups until journal removal, making recovery itself repeatable.
                regular_file(backup)
                staged.unlink(missing_ok=True)
                os.link(backup, staged, follow_symlinks=False)
                os.replace(staged, target)
            else:
                target.unlink(missing_ok=True)
            sync_directory(target.parent)
    # Once recovery/commit is durable, backup removal is only housekeeping.
    path.unlink()
    sync_directory(prefix)
    for item in journal["files"]:
        _, staged, backup = transaction_paths(prefix, item, transaction_id)
        staged.unlink(missing_ok=True)
        backup.unlink(missing_ok=True)


def install(package, prefix, uninstall=False, expected_manifest_sha256=None):
    safe_directory(prefix)
    lock_path = prefix / ".rustterm-install.lock"
    descriptor = os.open(lock_path, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+b") as lock:
        lock_info = os.fstat(lock.fileno())
        if not stat.S_ISREG(lock_info.st_mode) or lock_info.st_uid != os.geteuid():
            raise ValueError("Installation lock is not an owned regular file")
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        recover(prefix)
        if expected_manifest_sha256 is not None:
            # In-app updates pin the installation they inspected. Check under
            # the installer lock so a stale download cannot overwrite a newer
            # installation made by another process while it was downloading.
            marker = prefix / "share/doc/rustterm/build-info.json"
            regular_file(marker)
            if marker.stat().st_size > 16_384:
                raise ValueError("Installed manifest is too large")
            digest = hashlib.sha256(marker.read_bytes()).hexdigest()
            if digest != expected_manifest_sha256:
                raise ValueError("The installed version changed. Restart RustTerm before updating again.")
        prepared = []
        transaction_id = str(uuid.uuid4())
        journal_written = False
        try:
            # Check the entire package before replacing even the first installed file.
            if not uninstall:
                for source, _ in FILES.values():
                    regular_file(package / source)
                manifest = json.loads((package / "build-info.json").read_text(encoding="utf-8"))
                with (package / "bin/rustterm").open("rb") as binary:
                    digest = hashlib.file_digest(binary, "sha256").hexdigest()
                if manifest.get("binary_sha256") != digest:
                    raise ValueError("Package executable checksum does not match build-info.json")
            for relative, (source, mode) in FILES.items():
                target = destination(prefix, relative)
                item = {"relative": relative, "existed": target.exists()}
                _, staged, backup = transaction_paths(prefix, item, transaction_id)
                prepared.append(item)
                if item["existed"]:
                    os.link(target, backup, follow_symlinks=False)
                if not uninstall:
                    with staged.open("xb") as output:
                        if relative.endswith(".desktop"):
                            text = desktop_text((package / source).read_text(encoding="utf-8"), prefix / "bin/rustterm")
                            output.write(text.encode("utf-8"))
                        else:
                            with (package / source).open("rb") as source_file:
                                shutil.copyfileobj(source_file, output)
                        os.fchmod(output.fileno(), mode)
                        output.flush()
                        os.fsync(output.fileno())
                sync_directory(target.parent)
            journal = {"id": transaction_id, "state": "prepared", "files": prepared}
            # A failed fsync may follow a successful rename; inspect the actual journal.
            write_journal(prefix, journal)
            journal_written = True
            for item in prepared:
                target, staged, _ = transaction_paths(prefix, item, transaction_id)
                if uninstall:
                    target.unlink(missing_ok=True)
                else:
                    os.replace(staged, target)
                sync_directory(target.parent)
            journal["state"] = "committed"
            write_journal(prefix, journal)
            recover(prefix)  # Only housekeeping when state == committed.
        except BaseException:
            if journal_written or (prefix / JOURNAL).exists():
                recover(prefix)
            else:
                for item in prepared:
                    _, staged, backup = transaction_paths(prefix, item, transaction_id)
                    staged.unlink(missing_ok=True)
                    backup.unlink(missing_ok=True)
            raise


def main():
    parser = argparse.ArgumentParser(description="Install RustTerm; configuration and credentials are retained on uninstall.")
    parser.add_argument("--prefix", help="Absolute installation directory (default: ~/.local)")
    parser.add_argument("--uninstall", action="store_true")
    parser.add_argument("--expected-manifest-sha256", help=argparse.SUPPRESS)
    options = parser.parse_args()
    raw_prefix = options.prefix or (str(Path(os.environ["HOME"]) / ".local") if os.environ.get("HOME") else None)
    if not raw_prefix or not Path(raw_prefix).is_absolute():
        parser.error("An absolute --prefix is required when HOME is not set")
    prefix = Path(raw_prefix).resolve()
    if prefix == Path("/") or "=" in str(prefix) or any(ord(c) < 32 or ord(c) == 127 for c in str(prefix)):
        parser.error("Choose a directory other than /, without = or control characters")
    def interrupt(_signal, _frame):
        raise InterruptedError("Installation interrupted; the transaction will be recovered")
    signal.signal(signal.SIGTERM, interrupt)
    try:
        install(Path(__file__).resolve().parent, prefix, options.uninstall,
                options.expected_manifest_sha256)
    except (OSError, ValueError, KeyError, TypeError, KeyboardInterrupt) as error:
        print(f"RustTerm installation failed: {error}", file=sys.stderr)
        print("If rollback was interrupted, run this installer again with the same prefix to recover.", file=sys.stderr)
        return 1
    print(f"{'Removed' if options.uninstall else 'Installed'} RustTerm in {prefix}; configuration and credentials retained.")
    # Cache refresh is best-effort and bounded; it cannot invalidate committed files.
    if shutil.which("update-desktop-database"):
        with contextlib.suppress(OSError, subprocess.SubprocessError):
            subprocess.run(["update-desktop-database", str(prefix / "share/applications")], check=False, timeout=10)
    return 0


if __name__ == "__main__":
    sys.exit(main())
