"""Focused regression tests for tools/jean-desktop (Items 1, 5, 7, 15, 16)."""

import ctypes
import errno
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import queue
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import audio_meter
import desktop_panels
import gemini_listening
import model_settings
import voice_bench
import workspace_trash

from PyQt6.QtWidgets import QApplication, QMessageBox


def load_jean_desktop():
    spec = importlib.util.spec_from_file_location("jean_desktop_mod", HERE / "jean-desktop.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


JEAN_DESKTOP = load_jean_desktop()


class FakeGeminiConnection:
    """Fake BidiGenerateContent websocket that emits chunks before and after activityEnd."""

    def __init__(self, pre_end_chunks, post_end_chunks, *, send_turn_complete=True):
        self.pre_end_chunks = list(pre_end_chunks)
        self.post_end_chunks = list(post_end_chunks)
        self.send_turn_complete = send_turn_complete
        self.sent = []
        self.incoming = queue.Queue()
        self.activity_ended = False

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def send(self, payload):
        message = json.loads(payload)
        self.sent.append(message)
        if "setup" in message:
            self.incoming.put(json.dumps({"setupComplete": {}}))
        elif "realtimeInput" in message:
            rt = message["realtimeInput"]
            if "audio" in rt and self.pre_end_chunks:
                chunk = self.pre_end_chunks.pop(0)
                self.incoming.put(json.dumps({
                    "serverContent": {"inputTranscription": {"text": chunk}},
                }))
            elif "activityEnd" in rt:
                self.activity_ended = True
                while self.pre_end_chunks:
                    chunk = self.pre_end_chunks.pop(0)
                    self.incoming.put(json.dumps({
                        "serverContent": {"inputTranscription": {"text": chunk}},
                    }))
                for chunk in self.post_end_chunks:
                    self.incoming.put(json.dumps({
                        "serverContent": {"inputTranscription": {"text": chunk}},
                    }))
                if self.send_turn_complete:
                    self.incoming.put(json.dumps({
                        "serverContent": {"turnComplete": True},
                    }))

    def recv(self, timeout=None):
        try:
            return self.incoming.get(timeout=timeout or 0.01)
        except queue.Empty as exc:
            raise TimeoutError("timed out") from exc


class DesktopRegressionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = QApplication.instance() or QApplication([])

    def test_1_gemini_live_preserves_chunks_before_and_after_activity_end(self):
        fake = FakeGeminiConnection(
            pre_end_chunks=["Jean,", "open "],
            post_end_chunks=["Spotify", " and play jazz"],
        )
        live = gemini_listening.Listening(
            gemini_listening.MODEL,
            "AIza-dummy-test-key",
            connect=lambda *args, **kwargs: fake,
        )
        live.chunks.put("AAAA")
        live.chunks.put("BBBB")
        live.ended.set()
        transcript = live.result.result(timeout=5)
        self.assertEqual(transcript, "Jean, open Spotify and play jazz")
        self.assertTrue(fake.activity_ended)

        # Also verify when all chunks arrive before activityEnd and turnComplete arrives after.
        fake_pre_only = FakeGeminiConnection(
            pre_end_chunks=["Open PowerShell"],
            post_end_chunks=[],
        )
        live_pre = gemini_listening.Listening(
            gemini_listening.MODEL,
            "AIza-dummy-test-key",
            connect=lambda *args, **kwargs: fake_pre_only,
        )
        live_pre.chunks.put("AAAA")
        live_pre.ended.set()
        self.assertEqual(live_pre.result.result(timeout=5), "Open PowerShell")

    def test_gemini_transcribe_finalizes_without_turn_complete(self):
        fake = FakeGeminiConnection([], ["Open", " YouTube"], send_turn_complete=False)
        live = gemini_listening.Listening(
            gemini_listening.MODEL, "dummy", connect=lambda *args, **kwargs: fake,
        )
        live.ended.set()
        self.assertEqual(live.result.result(timeout=2), "Open YouTube")

    def test_5_delete_request_routing_strips_wake_address_and_requires_file_or_folder(self):
        self.assertEqual(
            JEAN_DESKTOP.trash_request("Jean, delete file notes.txt"),
            ("put", "notes.txt"),
        )
        self.assertEqual(
            JEAN_DESKTOP.trash_request("Hey Jean, please delete the folder old_drafts"),
            ("put", "old_drafts"),
        )
        self.assertEqual(
            JEAN_DESKTOP.trash_request("Jean, empty the trash"),
            ("clean", ""),
        )
        self.assertIsNone(
            JEAN_DESKTOP.trash_request("delete the unused function in app.py")
        )
        self.assertEqual(
            JEAN_DESKTOP.request_mode("smart", "Jean, delete file notes.txt"),
            "task",
        )
        self.assertEqual(
            JEAN_DESKTOP.request_mode("smart", "delete the unused function in app.py"),
            "task",
        )

        overlay = JEAN_DESKTOP.Overlay()
        try:
            with (
                mock.patch.object(overlay, "_handle_trash", wraps=overlay._handle_trash) as trash_spy,
                mock.patch.object(overlay.trash, "put", return_value="a" * 32) as put_mock,
                mock.patch.object(overlay, "_run_task") as run_task_mock,
            ):
                overlay._submit_request("smart", "Jean, delete file notes.txt")
                self.assertEqual(trash_spy.call_count, 1)
                run_task_mock.assert_not_called()

                overlay._submit_request("smart", "delete the unused function in app.py")
                run_task_mock.assert_called_once_with("delete the unused function in app.py")
        finally:
            overlay.close()

    def test_7_audio_selectors_capture_form_factors_unmeterable_output_and_capture_launcher(self):
        # Built-in (4), Bluetooth (3, 5, 6, 10), and USB (2, 4, 5, 10) inputs are accepted.
        for form, name in (
            (4, "Microphone Array (Realtek(R) Audio)"),
            (5, "Headset (WH-1000XM5 Hands-Free AG Audio)"),
            (6, "Handset (Bluetooth Hands-Free)"),
            (3, "Headphones (Bluetooth Audio Input)"),
            (2, "Line (Focusrite USB Audio)"),
            (10, "Microphone (USB PnP Audio Device)"),
        ):
            self.assertTrue(
                audio_meter.is_capture_input(19, form, name),
                f"Expected form={form} ({name}) to be accepted as a capture input",
            )

        # Output/digital form factors and loopback/mix virtual devices are rejected.
        for form, name in (
            (1, "Speakers (Realtek(R) Audio)"),
            (8, "SPDIF Interface"),
            (9, "HDMI Display Audio"),
            (10, "Stereo Mix (Realtek(R) Audio)"),
            (2, "What U Hear (Sound Blaster)"),
            (4, "Wave Out Mix"),
            (10, "CABLE Output Loopback"),
        ):
            self.assertFalse(
                audio_meter.is_capture_input(19, form, name),
                f"Expected form={form} ({name}) to be rejected",
            )

        # Verify AudioControls selector population with built-in, Bluetooth, and USB inputs.
        fake_meter = mock.MagicMock()
        capture_devices = [
            ("ep-builtin", "Microphone Array (Realtek(R) Audio)"),
            ("ep-bt", "Headset (Bluetooth Hands-Free)"),
            ("ep-usb", "Microphone (USB PnP Sound Device)"),
        ]
        render_devices = [
            ("ep-spk", "Speakers (Realtek(R) Audio)"),
            ("ep-hdmi", "HDMI (Display Audio)"),
        ]
        fake_meter.endpoints.side_effect = lambda cap: capture_devices if cap else render_devices
        fake_meter.default_endpoint.side_effect = lambda cap: "ep-usb" if cap else "ep-spk"
        fake_meter.volume_state.return_value = (75, False)

        from PyQt6.QtWidgets import QComboBox, QPushButton
        controls = desktop_panels.AudioControls(
            fake_meter,
            lambda cap: None,
            QPushButton("Wake"),
            QComboBox(),
            QComboBox(),
            lambda cap, ep: None,
        )
        controls.refresh()
        mic_combo = controls.selectors[True]
        self.assertEqual(
            [mic_combo.itemText(i) for i in range(mic_combo.count())],
            [name for _, name in capture_devices],
        )
        self.assertEqual(mic_combo.currentData(), "ep-usb")

        # Verify PlaybackMeter peak() handling when one output is unmeterable:
        # 1) Useful positive reading from another meterable output is preserved.
        # 2) When meterable outputs read 0.0 and an output is unmeterable, peak() returns None
        #    (never treating unknown playback as silence).
        meter = audio_meter.PlaybackMeter.__new__(audio_meter.PlaybackMeter)
        meter.refresh_at = float("inf")
        meter.meters = ["m1"]
        meter.complete = False  # One output failed Activate(IID_METER)

        with mock.patch.object(audio_meter, "method") as method_mock:
            def fake_read_positive(_m, out_ptr):
                ctypes.cast(out_ptr, ctypes.POINTER(ctypes.c_float))[0] = 0.42
                return 0

            method_mock.return_value = fake_read_positive
            self.assertAlmostEqual(meter.peak(), 0.42, places=5)

            def fake_read_zero(_m, out_ptr):
                ctypes.cast(out_ptr, ctypes.POINTER(ctypes.c_float))[0] = 0.0
                return 0

            method_mock.return_value = fake_read_zero
            self.assertIsNone(meter.peak())

            meter.complete = True
            self.assertEqual(meter.peak(), 0.0)

        # Verify PlaybackMeter.microphone() with built-in, Bluetooth, USB, loopback, and role mismatch.
        def check_mic_with_roles(role_specs, *, verify_all_roles):
            m = audio_meter.PlaybackMeter.__new__(audio_meter.PlaybackMeter)
            m.enumerator = 1
            m.ole = mock.MagicMock()

            def fake_method(interface, slot, result, *params):
                ptr = getattr(interface, "value", interface)
                if ptr == 1 and slot == 4:
                    def get_default(_enum, _flow, role, dev_ptr):
                        if role not in role_specs:
                            return -1
                        ctypes.cast(dev_ptr, ctypes.POINTER(audio_meter.POINTER))[0] = 100 + role
                        return 0
                    return get_default
                if ptr in (100, 101, 102) and slot == 4:
                    def open_store(dev, _mode, store_ptr):
                        dev_val = getattr(dev, "value", dev)
                        ctypes.cast(store_ptr, ctypes.POINTER(audio_meter.POINTER))[0] = dev_val + 100
                        return 0
                    return open_store
                if ptr in (200, 201, 202) and slot == 5:
                    role = ptr - 200
                    form_val, dev_name = role_specs[role]
                    buf = ctypes.create_unicode_buffer(dev_name)
                    open_buffers.append(buf)

                    def read_prop(_store, key_ptr, val_ptr):
                        key = ctypes.cast(key_ptr, ctypes.POINTER(audio_meter.PropertyKey))[0]
                        val = ctypes.cast(val_ptr, ctypes.POINTER(audio_meter.PropertyValue))[0]
                        if key.pid == audio_meter.FRIENDLY_NAME.pid:
                            val.vt = 31
                            val.value = ctypes.addressof(buf)
                        else:
                            val.vt = 19
                            val.value = form_val
                        ctypes.cast(val_ptr, ctypes.POINTER(audio_meter.PropertyValue))[0] = val
                        return 0
                    return read_prop
                return lambda *_a: 0

            open_buffers = []
            with (
                mock.patch.object(audio_meter, "method", side_effect=fake_method),
                mock.patch.object(audio_meter, "release", lambda _p: None),
            ):
                return m.microphone(verify_all_roles=verify_all_roles)

        for form_val, label in (
            (4, "Microphone Array (Built-in)"),
            (5, "Headset (Bluetooth Hands-Free)"),
            (2, "Line (USB Audio Interface)"),
            (10, "USB Condenser Microphone"),
        ):
            self.assertEqual(
                check_mic_with_roles({0: (form_val, label), 1: (form_val, label), 2: (form_val, label)}, verify_all_roles=True),
                label,
            )

        with self.assertRaisesRegex(OSError, "Set a microphone or headset mic"):
            check_mic_with_roles({1: (10, "Stereo Mix (Realtek)")}, verify_all_roles=False)

        with self.assertRaisesRegex(OSError, "Set a microphone or headset mic"):
            check_mic_with_roles({1: (1, "Speakers (Realtek)")}, verify_all_roles=False)

        with self.assertRaisesRegex(OSError, "Default Communication Device"):
            check_mic_with_roles(
                {
                    0: (4, "Microphone Array"),
                    1: (4, "Microphone Array"),
                    2: (5, "Headset (Bluetooth)"),
                },
                verify_all_roles=True,
            )

        # Check sounddevice using the exact Python launcher JEAN uses for voice_capture.py.
        argv, cwd = voice_bench.capture_command(
            HERE / "dummy.wav", HERE / "dummy.stop"
        )
        launcher_prefix = argv[: argv.index("python") + 1]
        proc = subprocess.run(
            [*launcher_prefix, "-c", "import sounddevice, numpy; print(sounddevice.__version__)"],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        )
        self.assertEqual(proc.stdout.strip(), "0.5.6")

    def test_15_overlay_approvals_off_thread_calls_and_credential_isolation(self):
        overlay = JEAN_DESKTOP.Overlay()
        try:
            # 1) Browser and window-list operations route through _run_task (the desktop approval bridge).
            with mock.patch.object(overlay, "_run_task") as run_task_mock:
                overlay._browse("https://example.com/")
                run_task_mock.assert_called_once_with("/browse https://example.com/")
                run_task_mock.reset_mock()

                overlay._list_desktop_windows()
                run_task_mock.assert_called_once_with(
                    "/windows",
                    extra_env={"CUA_DRIVER_RS_TELEMETRY_ENABLED": "0"},
                )

            # 2) Test the overlay approval dialog and response bridging.
            sent_inputs = []
            overlay.task_proc = object()
            overlay.cancel_sent = False
            overlay.yolo.setChecked(False)
            with mock.patch.object(overlay, "_send_task_input", side_effect=sent_inputs.append):
                overlay._ask_approval({
                    "id": 7,
                    "summary": "Open https://example.com/ in controlled browser",
                    "operation": "drive_browser",
                    "tool": "browser",
                    "tier": "normal",
                    "capabilities": ["browser_automation"],
                    "timeout_ms": 30000,
                })
                self.assertIsNotNone(overlay.approval)
                self.assertIn("AWAITING APPROVAL", overlay.status.text())
                overlay._answer_approval("task approved", overlay.approval)
                self.app.processEvents()
                self.assertEqual(
                    sent_inputs,
                    [{"kind": "approve", "id": 7, "allow": True}],
                )

            # 3) Inspect child environment for app discovery using dummy keys and verify off-thread execution.
            overlay.model_profiles = {
                "main": {
                    **model_settings.DEFAULTS["main"],
                    "provider": "openai_compatible",
                    "api_key_env": "CUSTOM_MAIN_API_KEY",
                },
                "decision": {
                    **model_settings.DEFAULTS["decision"],
                    "mode": "shadow",
                    "api_key_env": "CUSTOM_DECISION_API_KEY",
                },
                "listening": model_settings.DEFAULTS["listening"].copy(),
            }
            main_thread = threading.current_thread()
            captured_calls = []
            done_event = threading.Event()

            def fake_jean(argv, timeout, cancel=None, env=None, use_saved_models=True):
                captured_calls.append({
                    "argv": argv,
                    "thread": threading.current_thread(),
                    "env": env,
                    "use_saved_models": use_saved_models,
                })
                done_event.set()
                return "Gmail.AppID\n", ""

            with (
                mock.patch.dict(os.environ, {
                    model_settings.MAIN_KEY: "sk-dummy-main-12345",
                    model_settings.DECISION_KEY: "sk-dummy-decision-12345",
                    "JEAN_MODEL_API_KEY": "sk-dummy-default-main",
                    "JEAN_DECISION_API_KEY": "sk-dummy-default-decision",
                    "CUSTOM_MAIN_API_KEY": "sk-dummy-custom-main",
                    "CUSTOM_DECISION_API_KEY": "sk-dummy-custom-decision",
                }),
                mock.patch.object(overlay, "_jean", side_effect=fake_jean),
                mock.patch.object(JEAN_DESKTOP.subprocess, "Popen") as popen_mock,
            ):
                overlay._open_gmail()
                self.assertTrue(done_event.wait(timeout=3))
                self.app.processEvents()
                self.assertEqual(len(captured_calls), 1)
                call = captured_calls[0]
                self.assertIsNot(call["thread"], main_thread)
                self.assertFalse(call["use_saved_models"])
                for forbidden in (
                    model_settings.MAIN_KEY,
                    model_settings.DECISION_KEY,
                    "JEAN_MODEL_API_KEY",
                    "JEAN_DECISION_API_KEY",
                    "CUSTOM_MAIN_API_KEY",
                    "CUSTOM_DECISION_API_KEY",
                ):
                    self.assertNotIn(forbidden, call["env"])
                    for value in call["env"].values():
                        self.assertNotIn("sk-dummy-", str(value))

                # Also check that _handle_memory runs off the Qt thread.
                captured_calls.clear()
                done_event.clear()
                self.assertTrue(overlay._handle_memory("list memories"))
                self.assertTrue(done_event.wait(timeout=3))
                self.assertIsNot(captured_calls[0]["thread"], main_thread)
                self.assertFalse(captured_calls[0]["use_saved_models"])
        finally:
            overlay.close()

    def test_16_workspace_trash_reliability(self):
        with tempfile.TemporaryDirectory(prefix="jean-trash-test-") as tmp:
            base = Path(tmp)
            ws = base / "workspace"
            ws.mkdir()

            # 1) Non-canonical data path (contains '..').
            non_canonical = base / "nested" / ".." / "trash-store"
            trash = workspace_trash.WorkspaceTrash(non_canonical)

            # 2) Simulated cross-drive move (EXDEV / winerror 17) during put and restore.
            disposable = ws / "subdir" / "deep" / "note.txt"
            disposable.parent.mkdir(parents=True)
            disposable.write_text("recoverable data", encoding="utf-8")

            def exdev_rename(self_path, target):
                err = OSError(errno.EXDEV, "Cross-device link")
                err.winerror = 17
                raise err

            with mock.patch.object(Path, "rename", exdev_rename):
                entry_id = trash.put(ws, "subdir/deep/note.txt")
            self.assertFalse(disposable.exists())
            self.assertEqual(len(trash.list()), 1)

            # 3) Missing restore parents: remove subdir/deep and subdir before restoring.
            (ws / "subdir" / "deep").rmdir()
            (ws / "subdir").rmdir()

            with mock.patch.object(Path, "rename", exdev_rename):
                restored = trash.restore(entry_id)
            self.assertEqual(restored, disposable.resolve())
            self.assertEqual(disposable.read_text(encoding="utf-8"), "recoverable data")

            # 4) Failed cross-drive copy during put preserves original workspace file and leaves no trash entry.
            fail_file = ws / "keep_me.txt"
            fail_file.write_text("must survive failed copy", encoding="utf-8")
            with (
                mock.patch.object(Path, "rename", exdev_rename),
                mock.patch.object(workspace_trash.shutil, "copy2", side_effect=OSError("disk full")),
            ):
                with self.assertRaises(OSError):
                    trash.put(ws, "keep_me.txt")
            self.assertTrue(fail_file.exists())
            self.assertEqual(fail_file.read_text(encoding="utf-8"), "must survive failed copy")
            self.assertEqual(trash.list(), [])

            # 5) Occupied restore path & failed copy during restore both preserve the trash entry.
            entry_id = trash.put(ws, "keep_me.txt")
            fail_file.write_text("new occupant", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                trash.restore(entry_id)
            self.assertEqual(fail_file.read_text(encoding="utf-8"), "new occupant")
            self.assertEqual(len(trash.list()), 1)

            fail_file.unlink()
            with (
                mock.patch.object(Path, "rename", exdev_rename),
                mock.patch.object(workspace_trash.shutil, "copy2", side_effect=OSError("copy interrupted")),
            ):
                with self.assertRaises(OSError):
                    trash.restore(entry_id)
            self.assertFalse(fail_file.exists())
            self.assertEqual(len(trash.list()), 1)
            # Now restore succeeds cleanly.
            trash.restore(entry_id)
            self.assertEqual(fail_file.read_text(encoding="utf-8"), "must survive failed copy")

            # 6) Read-only file and read-only folder purge.
            ro_dir = ws / "ro_folder"
            ro_dir.mkdir()
            ro_child = ro_dir / "pack.idx"
            ro_child.write_text("readonly pack", encoding="utf-8")
            os.chmod(ro_child, stat.S_IREAD)
            trash.put(ws, "ro_folder")
            os.chmod(fail_file, stat.S_IREAD)
            trash.put(ws, "keep_me.txt")
            self.assertEqual(trash.purge(everything=True), 2)
            self.assertEqual(trash.list(), [])


if __name__ == "__main__":
    unittest.main()
