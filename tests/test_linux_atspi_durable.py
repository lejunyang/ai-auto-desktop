"""Durable integration for the real Linux AT-SPI process provider."""

from __future__ import annotations

import json
import os
from pathlib import Path
import sys
import tempfile
import unittest

from ai_auto_desktop.compiler import load_descriptor
from ai_auto_desktop.durable import DurableExecutor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.journal import JournalStore, RunStatus
from ai_auto_desktop.plugin import ProcessPlugin


PROJECT_ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = PROJECT_ROOT / "examples/workflows/linux-durable-session-inspection.yaml"
PLUGIN = PROJECT_ROOT / "plugins/linux_atspi/run.sh"


class LinuxAtspiDurableTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.path = Path(self.temporary.name) / "runs.sqlite3"
        self.store = JournalStore(self.path)
        self.addCleanup(self.store.close)
        self.workflow = load_descriptor(WORKFLOW)

    @unittest.skipUnless(
        sys.platform.startswith("linux"), "requires the Linux process provider"
    )
    def test_unavailable_process_provider_persists_public_session_projection(
        self,
    ) -> None:
        environment = os.environ.copy()
        environment["XDG_SESSION_TYPE"] = "wayland"
        environment["XDG_CURRENT_DESKTOP"] = "CI"
        environment.pop("DISPLAY", None)
        environment.pop("DBUS_SESSION_BUS_ADDRESS", None)
        environment.pop("AT_SPI_BUS_ADDRESS", None)
        outcome = DurableExecutor(
            self.store, owner_id="linux-reader",
            durable_action_mode="read-only",
        ).start(
            self.workflow, run_id="linux-session",
            plugins={
                "desktop.linux_atspi": ProcessPlugin(
                    [str(PLUGIN)], env=environment,
                    name="desktop.linux_atspi",
                )
            },
            granted_permissions=["desktop.observe"],
        )

        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(
            set(outcome.run.output["session"]),
            {"backend", "session_type", "desktop"},
        )
        self.assertEqual(
            outcome.run.output["session"],
            {
                "backend": "linux_atspi_unavailable",
                "session_type": "wayland",
                "desktop": "CI",
            },
        )
        persisted = json.dumps(
            {
                "run": outcome.run.to_dict(),
                "events": [
                    event.to_dict()
                    for event in self.store.list_events("linux-session")
                ],
            },
            sort_keys=True,
        )
        for forbidden in ("applications", "snapshot_id", "node_id"):
            self.assertNotIn(forbidden, persisted)

    def test_default_durable_mode_still_rejects_real_provider_action(self) -> None:
        with self.assertRaises(AutomationError) as rejected:
            DurableExecutor(self.store).start(
                self.workflow, plugins={"desktop.linux_atspi": [str(PLUGIN)]},
                granted_permissions=["desktop.observe"],
            )
        self.assertEqual(rejected.exception.code, "DURABLE.UNSUPPORTED_PLAN")
        self.assertEqual(self.store.list_runs(), [])


if __name__ == "__main__":
    unittest.main()
