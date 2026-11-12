"""Recoverable, same-volume moves initiated by the J.E.A.N. overlay."""

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


class WorkspaceTrash:
    def __init__(self, directory):
        self.directory = directory.absolute()
        self.lock = threading.Lock()

    def _prepare(self):
        self.directory.mkdir(parents=True, exist_ok=True)
        if self.directory.resolve() != self.directory:
            raise ValueError("The trash directory must not redirect to another location.")
        _plain(self.directory)

    def _workspace_path(self, workspace, name):
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
            try:
                with manifest.open("x", encoding="utf-8") as handle:
                    json.dump({"source": str(source), "workspace": str(workspace.resolve()),
                               "deleted_at": time.time()}, handle)
                    handle.flush()
                    os.fsync(handle.fileno())
                source.rename(entry / "payload")
            except OSError:
                manifest.unlink(missing_ok=True)
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
            source = self._workspace_path(Path(record["workspace"]), record["source"])
            (entry / "payload").rename(source)
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
                    payload = entry / "payload"
                    if stat.S_ISDIR(_plain(payload).st_mode):
                        shutil.rmtree(payload)
                    else:
                        payload.unlink()
                    (entry / "entry.json").unlink()
                    entry.rmdir()
                    removed += 1
            return removed
