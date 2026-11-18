"""Recoverable workspace trash moves initiated by the J.E.A.N. overlay."""

import errno
import json
import os
import re
import shutil
import stat
import threading
import time
import uuid
from datetime import datetime
from pathlib import Path

RETENTION_SECONDS = 14 * 24 * 60 * 60
ENTRY_ID = re.compile(r"[0-9a-f]{32}")


def _plain(path):
    info = path.lstat()
    if info.st_file_attributes & stat.FILE_ATTRIBUTE_REPARSE_POINT:
        raise ValueError(f"Windows links and junctions are excluded: {path}")
    return info


def _plain_tree(path):
    info = _plain(path)
    if stat.S_ISDIR(info.st_mode):
        for entry in path.iterdir():
            _plain_tree(entry)
    return info


def _clear_readonly(func, target, _exc):
    os.chmod(target, stat.S_IWRITE | stat.S_IREAD)
    func(target)


def _remove_path(path):
    info = _plain(path)
    if stat.S_ISDIR(info.st_mode):
        os.chmod(path, stat.S_IWRITE | stat.S_IREAD)
        shutil.rmtree(path, onexc=_clear_readonly)
    else:
        if not (info.st_mode & stat.S_IWRITE):
            os.chmod(path, stat.S_IWRITE | stat.S_IREAD)
        path.unlink()


def _is_cross_device(exc):
    return exc.errno == errno.EXDEV or getattr(exc, "winerror", None) == 17


def _move_plain(source, destination, *, copy_file=None):
    if copy_file is None:
        copy_file = shutil.copy2
    if destination.exists() or destination.is_symlink():
        raise FileExistsError(f"Destination already exists: {destination}")
    info = _plain(source)
    try:
        source.rename(destination)
        return
    except OSError as exc:
        if not _is_cross_device(exc):
            raise
    _plain_tree(source)
    is_dir = stat.S_ISDIR(info.st_mode)
    try:
        if is_dir:
            shutil.copytree(
                source,
                destination,
                symlinks=False,
                copy_function=copy_file,
            )
        else:
            copy_file(source, destination)
    except Exception:
        if destination.exists() or destination.is_symlink():
            _remove_path(destination)
        raise
    try:
        _remove_path(source)
    except Exception:
        if not is_dir and (source.exists() or source.is_symlink()):
            if destination.exists() or destination.is_symlink():
                _remove_path(destination)
        raise


class WorkspaceTrash:
    def __init__(self, directory):
        self._raw_directory = Path(os.path.abspath(directory))
        self.directory = self._raw_directory
        self.lock = threading.Lock()

    def _prepare(self):
        self._raw_directory.mkdir(parents=True, exist_ok=True)
        _plain(self._raw_directory)
        if self._raw_directory.parent.exists():
            _plain(self._raw_directory.parent)
        resolved = self._raw_directory.resolve(strict=True)
        _plain(resolved)
        self.directory = resolved

    def _workspace_path(self, workspace, name, *, allow_missing_parents=False):
        root = workspace.resolve(strict=True)
        path = Path(os.path.abspath(root / name))
        if not path.is_relative_to(root) or path == root:
            raise ValueError("Choose an item inside the workspace.")
        if path.is_relative_to(self.directory) or self.directory.is_relative_to(path):
            raise ValueError("The workspace trash cannot contain itself.")
        current = root
        for part in path.relative_to(root).parts:
            if ":" in part or part.endswith((".", " ")):
                raise ValueError("Use an ordinary file or folder path.")
            current /= part
            if current != path:
                if allow_missing_parents and not current.exists() and not current.is_symlink():
                    continue
                _plain(current)
        return path

    def put(self, workspace, name):
        with self.lock:
            self._prepare()
            source = self._workspace_path(workspace, name)
            _plain(source)
            entry = self.directory / uuid.uuid4().hex
            entry.mkdir()
            manifest = entry / "entry.json"
            payload = entry / "payload"
            try:
                with manifest.open("x", encoding="utf-8") as handle:
                    json.dump({"source": str(source), "workspace": str(workspace.resolve()),
                               "deleted_at": time.time()}, handle)
                    handle.flush()
                    os.fsync(handle.fileno())
                _move_plain(source, payload)
            except Exception:
                if not payload.exists() or (source.is_file() and not source.is_symlink()):
                    if payload.exists() or payload.is_symlink():
                        _remove_path(payload)
                    manifest.unlink(missing_ok=True)
                    if entry.exists():
                        entry.rmdir()
                raise
            return entry.name

    def _entry(self, entry_id):
        if not ENTRY_ID.fullmatch(entry_id):
            raise ValueError("Enter the full trash ID shown by /trash list.")
        entry = self.directory / entry_id
        _plain(entry)
        _plain(entry / "entry.json")
        _plain(entry / "payload")
        record = json.loads((entry / "entry.json").read_text(encoding="utf-8"))
        return entry, record

    def _entries(self):
        for path in self.directory.iterdir():
            if ENTRY_ID.fullmatch(path.name):
                try:
                    entry, record = self._entry(path.name)
                    if (isinstance(record["deleted_at"], (int, float))
                            and isinstance(record["source"], str)):
                        yield entry, record
                except (OSError, ValueError, KeyError, TypeError):
                    continue

    def list(self):
        with self.lock:
            self._prepare()
            return [
                f'{entry.name}  {datetime.fromtimestamp(record["deleted_at"]):%Y-%m-%d}  '
                f'{record["source"]}'
                for entry, record in self._entries()
            ]

    def restore(self, entry_id):
        with self.lock:
            self._prepare()
            entry, record = self._entry(entry_id)
            workspace = Path(record["workspace"])
            source = self._workspace_path(
                workspace, record["source"], allow_missing_parents=True
            )
            if source.exists() or source.is_symlink():
                raise FileExistsError(f"Restore destination already exists: {source}")
            source.parent.mkdir(parents=True, exist_ok=True)
            source = self._workspace_path(workspace, record["source"])
            _move_plain(entry / "payload", source)
            (entry / "entry.json").unlink()
            entry.rmdir()
            return source

    def purge(self, *, everything=False):
        with self.lock:
            self._prepare()
            cutoff = time.time() - RETENTION_SECONDS
            removed = 0
            for entry, record in self._entries():
                if everything or record["deleted_at"] <= cutoff:
                    _remove_path(entry / "payload")
                    (entry / "entry.json").unlink()
                    entry.rmdir()
                    removed += 1
            return removed
