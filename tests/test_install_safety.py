from pathlib import Path
import ast
import importlib.util
from unittest import mock
import subprocess
import shlex
import sys
import tempfile
import unittest

INSTALLER=Path(__file__).resolve().parents[1]/"tools/install.py"
SPEC=importlib.util.spec_from_file_location("snipchord_install", INSTALLER)
INSTALL_MODULE=importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(INSTALL_MODULE)


class InstallSafetyTests(unittest.TestCase):
    def test_invalid_autostart_prefix_does_not_write(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix=Path(directory)/"untouched"
            result=subprocess.run([sys.executable,str(INSTALLER),"--prefix",str(prefix),"--autostart"],capture_output=True)
            self.assertNotEqual(result.returncode,0)
            self.assertFalse(prefix.exists())

    def test_existing_desktop_entry_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix=Path(directory)
            desktop=prefix/"share/applications/io.github.shane.snipchord.desktop"
            desktop.parent.mkdir(parents=True)
            desktop.write_text("user-owned")
            result=subprocess.run([sys.executable,str(INSTALLER),"--prefix",str(prefix)],capture_output=True)
            self.assertNotEqual(result.returncode,0)
            self.assertEqual(desktop.read_text(),"user-owned")
            self.assertFalse((prefix/"share/snipchord").exists())

    def test_existing_launcher_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix=Path(directory)
            launcher=prefix/"bin/snipchord"
            launcher.parent.mkdir(parents=True)
            launcher.write_bytes(b"user-owned")
            result=subprocess.run([sys.executable,str(INSTALLER),"--prefix",str(prefix)],capture_output=True)
            self.assertNotEqual(result.returncode,0)
            self.assertEqual(launcher.read_bytes(),b"user-owned")
            self.assertFalse((prefix/"share/snipchord").exists())

    def test_existing_icon_is_preserved(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix=Path(directory)
            icon=prefix/"share/icons/hicolor/scalable/apps/snipchord.svg"
            icon.parent.mkdir(parents=True)
            icon.write_bytes(b"user-owned")
            result=subprocess.run([sys.executable,str(INSTALLER),"--prefix",str(prefix)],capture_output=True)
            self.assertNotEqual(result.returncode,0)
            self.assertEqual(icon.read_bytes(),b"user-owned")
            self.assertFalse((prefix/"share/snipchord").exists())

    def test_binary_replacement_is_atomic_while_old_binary_runs(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix=Path(directory)
            launcher=prefix/"bin/snipchord"
            launcher.parent.mkdir(parents=True)
            INSTALL_MODULE.install_binary(Path(sys.executable), launcher)
            old_inode=launcher.stat().st_ino
            process=subprocess.Popen([str(launcher),"-c","import time; time.sleep(5)"])
            try:
                INSTALL_MODULE.install_binary(Path(sys.executable), launcher)
                self.assertNotEqual(launcher.stat().st_ino,old_inode)
                self.assertIsNone(process.poll())
            finally:
                process.terminate()
                process.wait(timeout=5)

    def test_shortcuts_preserve_unrelated_entries_and_are_idempotent(self):
        unrelated = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/"
        edited_managed = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom7/"
        state = {
            "paths": [unrelated, edited_managed],
            "shell": {
                "screenshot": ["<Primary><Shift><Alt>numbersign", "<Super>Print"],
                "screenshot-window": ["<Primary><Shift><Alt>percent"],
            },
            "entries": {
                unrelated: {
                    "name": "My existing shortcut",
                    "command": "my-command --keep",
                    "binding": "<Super>K",
                },
                edited_managed: {
                    "name": "A label I edited",
                    "command": "/old/location/snipchord --region --clipboard",
                    "binding": "<Super>4",
                },
            },
        }
        calls = []

        def fake_gsettings(*arguments):
            calls.append(arguments)
            operation, schema, key = arguments[:3]
            if schema == INSTALL_MODULE.GNOME_SHELL_KEYBINDINGS_SCHEMA:
                if operation == "get":
                    return repr(state["shell"][key])
                state["shell"][key] = ast.literal_eval(arguments[3])
                return ""
            if schema == INSTALL_MODULE.MEDIA_KEYS_SCHEMA:
                if operation == "get":
                    self.assertEqual(key, INSTALL_MODULE.CUSTOM_KEYBINDINGS_KEY)
                    return repr(state["paths"])
                self.assertEqual(operation, "set")
                state["paths"] = ast.literal_eval(arguments[3])
                return ""

            path = schema.split(":", 1)[1]
            state["entries"].setdefault(path, {})
            if operation == "get":
                return repr(state["entries"][path].get(key, ""))
            self.assertEqual(operation, "set")
            state["entries"][path][key] = ast.literal_eval(arguments[3])
            return ""

        launcher = Path("/tmp/snipchord prefix/bin/snipchord")
        with mock.patch.object(INSTALL_MODULE, "_gsettings", side_effect=fake_gsettings):
            first_paths = INSTALL_MODULE.configure_shortcuts(launcher)
            first_call_count = len(calls)
            second_paths = INSTALL_MODULE.configure_shortcuts(launcher)

        self.assertEqual(first_paths, second_paths)
        self.assertEqual(state["paths"], list(first_paths))
        self.assertEqual(state["paths"][0], unrelated)
        self.assertEqual(state["entries"][unrelated], {
            "name": "My existing shortcut",
            "command": "my-command --keep",
            "binding": "<Super>K",
        })
        self.assertEqual(state["shell"]["screenshot"], ["<Super>Print"])
        self.assertEqual(state["shell"]["screenshot-window"], ["<Primary><Shift><Alt>percent"])
        self.assertEqual(len(first_paths), 5)
        self.assertEqual(len(state["paths"]), len(set(state["paths"])))
        self.assertEqual(
            state["entries"][edited_managed]["command"],
            f"{shlex.quote(str(launcher.resolve()))} --region --clipboard",
        )
        self.assertEqual(state["entries"][edited_managed]["binding"], "<Control><Alt><Shift>4")
        self.assertGreater(len(calls), first_call_count)
        self.assertEqual(state["entries"][state["paths"][2]]["name"], "SnipChord Region to File")
        self.assertEqual(state["entries"][state["paths"][3]]["binding"], "<Control><Alt><Shift>3")
        self.assertEqual(state["entries"][state["paths"][4]]["binding"], "<Alt><Shift>3")

    def test_shortcuts_do_not_reuse_unrelated_snipchord_command(self):
        unrelated = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/"
        state = {
            "paths": [unrelated],
            "shell": {"screenshot": []},
            "entries": {
                unrelated: {
                    "name": "Open SnipChord preferences",
                    "command": "/usr/bin/snipchord --preferences",
                    "binding": "<Super>P",
                },
            },
        }

        def fake_gsettings(*arguments):
            operation, schema, key = arguments[:3]
            if schema == INSTALL_MODULE.GNOME_SHELL_KEYBINDINGS_SCHEMA:
                if operation == "get":
                    return repr(state["shell"][key])
                state["shell"][key] = ast.literal_eval(arguments[3])
                return ""
            if schema == INSTALL_MODULE.MEDIA_KEYS_SCHEMA:
                if operation == "get":
                    return repr(state["paths"])
                state["paths"] = ast.literal_eval(arguments[3])
                return ""
            path = schema.split(":", 1)[1]
            state["entries"].setdefault(path, {})
            if operation == "get":
                return repr(state["entries"][path].get(key, ""))
            state["entries"][path][key] = ast.literal_eval(arguments[3])
            return ""

        with mock.patch.object(INSTALL_MODULE, "_gsettings", side_effect=fake_gsettings):
            INSTALL_MODULE.configure_shortcuts(Path("/tmp/snipchord/bin/snipchord"))

        self.assertEqual(state["paths"][0], unrelated)
        self.assertEqual(state["entries"][unrelated]["binding"], "<Super>P")
        self.assertEqual(len(state["paths"]), 5)

    def test_legacy_region_binding_is_reused(self):
        paths = [
            f"/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom{index}/"
            for index in range(5)
        ]
        legacy_path = paths[4]
        state = {
            "paths": paths,
            "shell": {
                "screenshot": ["<Primary><Shift><Alt>numbersign", "<Super>Print"],
                "screenshot-window": ["<Primary><Shift><Alt>percent"],
            },
            "entries": {
                path: {
                    "name": "User shortcut" if path != legacy_path else "SnipChord — Capture Region",
                    "command": "user-command" if path != legacy_path else "/home/shane/.local/bin/snipchord --region",
                    "binding": "<Super>F1" if path != legacy_path else "<Primary><Alt><Shift>4",
                }
                for path in paths
            },
        }

        def fake_gsettings(*arguments):
            operation, schema, key = arguments[:3]
            if schema == INSTALL_MODULE.GNOME_SHELL_KEYBINDINGS_SCHEMA:
                if operation == "get":
                    return repr(state["shell"][key])
                state["shell"][key] = ast.literal_eval(arguments[3])
                return ""
            if schema == INSTALL_MODULE.MEDIA_KEYS_SCHEMA:
                if operation == "get":
                    return repr(state["paths"])
                state["paths"] = ast.literal_eval(arguments[3])
                return ""
            path = schema.split(":", 1)[1]
            state["entries"].setdefault(path, {})
            if operation == "get":
                return repr(state["entries"][path].get(key, ""))
            state["entries"][path][key] = ast.literal_eval(arguments[3])
            return ""

        with mock.patch.object(INSTALL_MODULE, "_gsettings", side_effect=fake_gsettings):
            configured_paths = INSTALL_MODULE.configure_shortcuts(Path("/tmp/snipchord/bin/snipchord"))

        self.assertEqual(configured_paths[4], legacy_path)
        self.assertEqual(len(configured_paths), 8)
        self.assertEqual(
            state["entries"][legacy_path]["command"],
            "/tmp/snipchord/bin/snipchord --region --clipboard",
        )
        self.assertEqual(state["entries"][legacy_path]["binding"], "<Control><Alt><Shift>4")
        self.assertEqual(state["shell"]["screenshot"], ["<Super>Print"])
        self.assertEqual(state["shell"]["screenshot-window"], ["<Primary><Shift><Alt>percent"])

    def test_gsettings_wrapper_uses_cli_operation_order(self):
        completed = subprocess.CompletedProcess(
            ["gsettings"],
            0,
            stdout="'value'\n",
            stderr="",
        )
        with mock.patch.object(INSTALL_MODULE.subprocess, "run", return_value=completed) as run:
            self.assertEqual(INSTALL_MODULE._gsettings("get", "schema", "key"), "'value'")
        run.assert_called_once_with(
            ["gsettings", "get", "schema", "key"],
            check=True,
            capture_output=True,
            text=True,
        )

    def test_gvariant_string_list_accepts_empty_type_annotation(self):
        self.assertEqual(INSTALL_MODULE._gvariant_string_list("@as []"), [])
        self.assertEqual(
            INSTALL_MODULE._gvariant_string_list("@as ['<Super>Print']"),
            ["<Super>Print"],
        )
