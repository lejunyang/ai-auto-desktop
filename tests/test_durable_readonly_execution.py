"""End-to-end contracts for durable read-only action execution."""

from __future__ import annotations

from copy import deepcopy
import json
from pathlib import Path
import threading
import tempfile
import unittest
from unittest import mock
import time

from ai_auto_desktop.cli import build_parser
from ai_auto_desktop.compiler import compile_descriptor
from ai_auto_desktop.durable import DurableExecutor
from ai_auto_desktop.errors import AutomationError
from ai_auto_desktop.journal import DesiredState, JournalConflictError, JournalStore, RunStatus
from ai_auto_desktop.plugin import PluginError, ProcessPlugin


RAW = "RAW-DURABLE-CANARY-19f6"
PUBLIC = {"input": "public", "output": "public", "error": "public"}


def plan(*, sensitive_input: bool = False, action_extra: dict[str, object] | None = None) -> object:
    step: dict[str, object] = {
        "id": "observe", "type": "action",
        "uses": "fixture.read@1", "with": {"query": "${{ inputs.query }}"},
        "effect": {"class": "read_only"},
        "risk": {"category": "observe", "level": "low"},
        "timeout": "2s", "attempt_timeout": "2s",
        "sensitivity": deepcopy(PUBLIC),
        "checkpoint": {"output": {"mode": "project", "fields": ["title"]}},
    }
    step.update(action_extra or {})
    return compile_descriptor({
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "Workflow",
        "metadata": {"name": "durable-readonly-e2e", "version": "1.0.0"},
        "inputs": {"query": {"schema": {"type": "string"}, "required": True, "sensitive": sensitive_input}},
        "outputs": {"title": {"value": "${{ steps.observe.output.title }}", "schema": {"type": "string"}}},
        "budgets": {"max_duration": "5s", "max_executed_steps": 2, "max_concurrency": 1},
        "steps": [step],
    })


def manifest(
    *, permissions: list[str] | None = None,
) -> dict[str, object]:
    value: dict[str, object] = {
        "apiVersion": "ai-auto-desktop.dev/v1alpha1",
        "kind": "CapabilityManifest",
        "metadata": {"name": "fixture", "version": "1.0.0"},
        "actions": {"read": {
            "contract_major": 1,
            "effect": {"default_class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
            "timeout": "2s",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "errors": [{"code": "FIXTURE.FAIL", "retryable": False, "effect": "not_applied"}],
            "sensitivity": deepcopy(PUBLIC),
            "durability": {"checkpoint_fields": {"title": {"pointer": "/safe/title", "schema": {"type": "string"}}}},
        }},
    }
    if permissions is not None:
        value["permissions"] = permissions
    return value


class StubPlugin(ProcessPlugin):
    def __init__(self, outcome: object | BaseException | None = None) -> None:
        super().__init__(["unused"], name="fixture")
        self.manifest_value = manifest()
        self.manifest = deepcopy(self.manifest_value)
        self.outcome = outcome if outcome is not None else {"safe": {"title": "ok"}, "secret": RAW}
        self.calls: list[object] = []

    def start(self, timeout: float | None = None) -> dict[str, object]:
        self.manifest = deepcopy(self.manifest_value)
        return deepcopy(self.manifest_value)

    def invoke(self, action: str, args: object, timeout: float | None = None) -> object:
        self.calls.append(deepcopy(args))
        if isinstance(self.outcome, BaseException):
            raise self.outcome
        return deepcopy(self.outcome)


class ManifestFailurePlugin(StubPlugin):
    def start(self, timeout: float | None = None) -> dict[str, object]:
        raise PluginError(
            "PLUGIN.HOST_PROTOCOL_ERROR", RAW,
            details={"stderr": RAW},
        )


class BlockingPlugin(StubPlugin):
    def __init__(self) -> None:
        super().__init__()
        self.invoked = threading.Event()
        self.release = threading.Event()

    def invoke(
        self, action: str, args: object, timeout: float | None = None
    ) -> object:
        self.calls.append(deepcopy(args))
        self.invoked.set()
        if not self.release.wait(5):
            raise AssertionError("test did not release blocking provider")
        return deepcopy(self.outcome)


class DurableReadOnlyExecutionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / "runs.sqlite3"
        self.store = JournalStore(self.path)
        self.addCleanup(self.store.close)

    def executor(self) -> DurableExecutor:
        return DurableExecutor(self.store, owner_id="worker", durable_action_mode="read-only")

    def test_default_deny_and_sensitive_descriptor_fail_before_creation(self) -> None:
        plugin = StubPlugin()
        with self.assertRaises(AutomationError):
            DurableExecutor(self.store).start(plan(), inputs={"query": "q"}, plugins={"fixture": plugin})
        with self.assertRaises(AutomationError) as rejected:
            self.executor().start(plan(sensitive_input=True), inputs={"query": RAW}, plugins={"fixture": plugin})
        self.assertEqual(rejected.exception.code, "DURABLE.SENSITIVE_DESCRIPTOR")
        self.assertEqual(self.store.list_runs(), [])

    def test_read_only_action_requires_file_backed_journal(self) -> None:
        with JournalStore(":memory:") as store:
            with self.assertRaises(AutomationError) as rejected:
                DurableExecutor(
                    store, durable_action_mode="read-only"
                ).start(
                    plan(), inputs={"query": "public"},
                    plugins={"fixture": StubPlugin()},
                )
            self.assertEqual(
                rejected.exception.code, "DURABLE.UNSUPPORTED_JOURNAL"
            )
            self.assertEqual(store.list_runs(), [])

    def test_preflight_provider_failure_is_redacted_before_run_creation(self) -> None:
        with self.assertRaises(AutomationError) as rejected:
            self.executor().start(
                plan(), inputs={"query": "public"},
                plugins={"fixture": ManifestFailurePlugin()},
            )
        self.assertEqual(
            rejected.exception.code, "DURABLE.ACTION_PREFLIGHT_FAILED"
        )
        self.assertNotIn(RAW, json.dumps(rejected.exception.to_dict()))
        self.assertEqual(self.store.list_runs(), [])

    def test_static_policy_failure_is_rejected_before_run_creation(self) -> None:
        manifests = []
        provider_permission = manifest(permissions=["desktop.observe"])
        manifests.append(provider_permission)
        action_permission = manifest()
        action_permission["actions"]["read"]["permissions"] = [
            "desktop.observe"
        ]
        manifests.append(action_permission)

        for index, manifest_value in enumerate(manifests):
            with self.subTest(index=index):
                plugin = StubPlugin()
                plugin.manifest_value = manifest_value
                plugin.manifest = deepcopy(plugin.manifest_value)
                with self.assertRaises(AutomationError) as rejected:
                    self.executor().start(
                        plan(), inputs={"query": "public"},
                        plugins={"fixture": plugin},
                    )
                self.assertEqual(
                    rejected.exception.code,
                    "DURABLE.ACTION_PREFLIGHT_FAILED",
                )
                self.assertEqual(self.store.list_runs(), [])
                self.assertEqual(plugin.calls, [])

    def test_preflight_defers_input_that_references_previous_action(self) -> None:
        common = {
            "type": "action",
            "uses": "fixture.read@1",
            "effect": {"class": "read_only"},
            "risk": {"category": "observe", "level": "low"},
            "timeout": "2s",
            "attempt_timeout": "2s",
            "sensitivity": deepcopy(PUBLIC),
            "checkpoint": {
                "output": {"mode": "project", "fields": ["title"]}
            },
        }
        workflow = compile_descriptor({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow",
            "metadata": {"name": "durable-readonly-chain"},
            "outputs": {
                "title": {"value": "${{ steps.second.output.title }}"}
            },
            "budgets": {
                "max_duration": "5s",
                "max_executed_steps": 3,
                "max_concurrency": 1,
            },
            "steps": [
                {"id": "first", "with": {"query": "seed"}, **common},
                {
                    "id": "second",
                    "with": {
                        "query": "${{ steps.first.output.title }}"
                    },
                    **common,
                },
            ],
        })
        plugin = StubPlugin()

        outcome = self.executor().start(
            workflow, run_id="action-chain", plugins={"fixture": plugin}
        )

        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(outcome.run.output, {"title": "ok"})
        self.assertEqual(plugin.calls, [{"query": "seed"}, {"query": "ok"}])

    def test_post_create_input_preparation_failure_is_terminal_and_releases_lease(self) -> None:
        plugin = StubPlugin()
        invalid = plan(action_extra={"with": {"query": "${{ 1 / 0 }}"}})

        outcome = self.executor().start(
            invalid, inputs={"query": "public"},
            run_id="prepare-failed", plugins={"fixture": plugin},
        )

        self.assertEqual(outcome.run.status, RunStatus.FAILED)
        self.assertIsNone(outcome.run.owner_id)
        self.assertEqual(
            outcome.run.error["code"],
            "DURABLE.ACTION_PREPARATION_FAILED",
        )
        self.assertEqual(plugin.calls, [])

    def test_action_preparation_finalization_intent_resumes_as_failure(self) -> None:
        plugin = StubPlugin()
        workflow = plan(action_extra={"with": {"query": "${{ 1 / 0 }}"}})
        original = self.store.append_event_with_checkpoint

        def crash_after_intent(
            run_id: str, event_type: str, payload: object,
            checkpoint: object, **kwargs: object,
        ) -> object:
            saved = original(
                run_id, event_type, payload, checkpoint, **kwargs
            )
            if (
                isinstance(checkpoint, dict)
                and isinstance(checkpoint.get("finalization"), dict)
                and checkpoint["finalization"].get("stage") == "intent"
            ):
                raise RuntimeError("crash-after-preparation-intent")
            return saved

        with mock.patch.object(
            self.store, "append_event_with_checkpoint",
            side_effect=crash_after_intent,
        ), self.assertRaises(AutomationError):
            self.executor().start(
                workflow, inputs={"query": "public"},
                run_id="prepare-intent", plugins={"fixture": plugin},
            )
        self.store._connection.execute(
            "UPDATE runs SET lease_expires_at = 0 WHERE run_id = ?",
            ("prepare-intent",),
        )

        outcome = DurableExecutor(
            self.store, owner_id="recovery", durable_action_mode="read-only"
        ).resume(
            "prepare-intent", workflow, plugins={"fixture": plugin}
        )

        self.assertEqual(outcome.run.status, RunStatus.FAILED)
        self.assertEqual(
            outcome.run.error["code"], "DURABLE.ACTION_PREPARATION_FAILED"
        )
        self.assertEqual(plugin.calls, [])

    def test_legacy_runner_factory_signature_still_works_in_default_mode(self) -> None:
        from ai_auto_desktop.runtime import WorkflowRunner
        simple = compile_descriptor({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow", "metadata": {"name": "legacy-factory"},
            "budgets": {"max_duration": "5s", "max_executed_steps": 2},
            "steps": [{"id": "done", "type": "return", "value": 1}],
        })
        def legacy_factory(descriptor: object, *, plugins: object = None, allow_scripts: bool = False, granted_permissions: object = None) -> WorkflowRunner:
            return WorkflowRunner(
                descriptor, plugins=plugins, allow_scripts=allow_scripts,
                granted_permissions=granted_permissions,
            )
        outcome = DurableExecutor(
            self.store, owner_id="legacy", runner_factory=legacy_factory
        ).start(simple, run_id="legacy")
        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)

    def test_success_persists_only_projection(self) -> None:
        plugin = StubPlugin()
        outcome = self.executor().start(plan(), inputs={"query": "public"}, run_id="ok", plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(outcome.run.output, {"title": "ok"})
        payload = json.dumps({"outcome": outcome.to_dict(), "events": [e.to_dict() for e in self.store.list_events("ok")]}, sort_keys=True)
        self.assertNotIn(RAW, payload)
        self.store._connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        for suffix in ("", "-wal", "-shm"):
            candidate = Path(str(self.path) + suffix)
            if candidate.exists():
                self.assertNotIn(RAW.encode(), candidate.read_bytes())

    def test_slow_provider_keeps_lease_and_cannot_be_claimed_twice(self) -> None:
        plugin = BlockingPlugin()
        ttl = 0.4
        outcome: list[object] = []
        failure: list[BaseException] = []

        def execute() -> None:
            try:
                with JournalStore(self.path) as store:
                    outcome.append(DurableExecutor(
                        store, owner_id="first", lease_ttl_seconds=ttl,
                        durable_action_mode="read-only",
                    ).start(
                        plan(), inputs={"query": "public"},
                        run_id="slow", plugins={"fixture": plugin},
                    ))
            except BaseException as exc:
                failure.append(exc)

        worker = threading.Thread(target=execute)
        worker.start()
        try:
            self.assertTrue(plugin.invoked.wait(3))
            time.sleep(ttl * 1.5)
            with self.assertRaises(JournalConflictError):
                self.store.claim_owner(
                    "slow", owner_id="second", ttl_seconds=ttl
                )
        finally:
            plugin.release.set()
            worker.join(3)

        self.assertFalse(worker.is_alive())
        self.assertEqual(failure, [])
        self.assertEqual(outcome[0].run.status, RunStatus.SUCCEEDED)
        self.assertEqual(len(plugin.calls), 1)

    def test_heartbeat_failure_is_visible_without_masking_provider_result(self) -> None:
        plugin = StubPlugin()
        failure = RuntimeError("SECRET-HEARTBEAT-FAILURE")
        keeper = mock.Mock()
        keeper.stop.return_value = failure

        with mock.patch(
            "ai_auto_desktop.durable._LeaseHeartbeatKeeper",
            return_value=keeper,
        ):
            outcome = self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="heartbeat-degraded", plugins={"fixture": plugin},
            )

        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(outcome.run.output, {"title": "ok"})
        self.assertEqual(len(plugin.calls), 1)
        events = self.store.list_events("heartbeat-degraded")
        self.assertEqual(
            sum(
                event.event_type == "run.lease_heartbeat_failed"
                for event in events
            ),
            1,
        )
        self.assertNotIn(
            str(failure),
            json.dumps([event.to_dict() for event in events]),
        )

    def test_heartbeat_loss_fences_old_owner_after_provider_returns(self) -> None:
        plugin = BlockingPlugin()
        ttl = 0.3
        failures: list[BaseException] = []
        keeper = mock.Mock()
        keeper.stop.return_value = RuntimeError("SECRET-KEEPER-FAILURE")

        def execute() -> None:
            try:
                with JournalStore(self.path) as store, mock.patch(
                    "ai_auto_desktop.durable._LeaseHeartbeatKeeper",
                    return_value=keeper,
                ):
                    DurableExecutor(
                        store, owner_id="old-owner",
                        lease_ttl_seconds=ttl,
                        durable_action_mode="read-only",
                    ).start(
                        plan(), inputs={"query": "public"},
                        run_id="fenced", plugins={"fixture": plugin},
                    )
            except BaseException as exc:
                failures.append(exc)

        worker = threading.Thread(target=execute)
        worker.start()
        try:
            self.assertTrue(plugin.invoked.wait(3))
            time.sleep(ttl * 1.5)
            replacement = self.store.claim_owner(
                "fenced", owner_id="new-owner", ttl_seconds=2
            )
        finally:
            plugin.release.set()
            worker.join(3)

        self.assertFalse(worker.is_alive())
        self.assertEqual(len(failures), 1)
        self.assertIsInstance(failures[0], AutomationError)
        self.assertEqual(
            failures[0].code, "DURABLE.LEASE_HEARTBEAT_FAILED"
        )
        self.assertEqual(failures[0].details["providerCompleted"], True)
        self.assertNotIn(
            str(keeper.stop.return_value),
            json.dumps(failures[0].to_dict()),
        )
        current = self.store.get_run("fenced")
        self.assertEqual(current.owner_id, replacement.owner_id)
        self.assertEqual(current.status, RunStatus.RUNNING)
        self.assertEqual(current.checkpoint["phase"], "action_intent")
        self.assertEqual(len(plugin.calls), 1)

    def test_heartbeat_start_failure_prevents_provider_dispatch(self) -> None:
        plugin = StubPlugin()
        keeper = mock.Mock()
        keeper.start.side_effect = RuntimeError("SECRET-START-FAILURE")

        with mock.patch(
            "ai_auto_desktop.durable._LeaseHeartbeatKeeper",
            return_value=keeper,
        ), self.assertRaises(AutomationError) as rejected:
            self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="heartbeat-start-failed",
                plugins={"fixture": plugin},
            )

        self.assertEqual(
            rejected.exception.code, "DURABLE.LEASE_HEARTBEAT_FAILED"
        )
        self.assertEqual(
            rejected.exception.details,
            {
                "runId": "heartbeat-start-failed",
                "stage": "before_dispatch",
                "providerCompleted": False,
            },
        )
        self.assertNotIn(
            str(keeper.start.side_effect),
            json.dumps(rejected.exception.to_dict()),
        )
        self.assertEqual(plugin.calls, [])

    def test_heartbeat_start_does_not_swallow_process_interrupts(self) -> None:
        for signal in (KeyboardInterrupt(), SystemExit(7), GeneratorExit()):
            with self.subTest(signal=type(signal).__name__), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "runs.sqlite3"
                with JournalStore(path) as store:
                    plugin = StubPlugin()
                    keeper = mock.Mock()
                    keeper.start.side_effect = signal
                    with mock.patch(
                        "ai_auto_desktop.durable._LeaseHeartbeatKeeper",
                        return_value=keeper,
                    ), self.assertRaises(type(signal)):
                        DurableExecutor(
                            store, owner_id="interrupt",
                            durable_action_mode="read-only",
                        ).start(
                            plan(), inputs={"query": "public"},
                            run_id="heartbeat-interrupt",
                            plugins={"fixture": plugin},
                        )
                    self.assertEqual(plugin.calls, [])

    def test_declared_error_is_redacted_before_persistence(self) -> None:
        plugin = StubPlugin(PluginError("FIXTURE.FAIL", RAW, details={"secret": RAW}))
        outcome = self.executor().start(plan(), inputs={"query": "public"}, run_id="failed", plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.FAILED)
        dump = json.dumps({"run": outcome.to_dict(), "events": [e.to_dict() for e in self.store.list_events("failed")]}, sort_keys=True)
        self.assertNotIn(RAW, dump)
        self.store._connection.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        for suffix in ("", "-wal", "-shm"):
            candidate = Path(str(self.path) + suffix)
            if candidate.exists():
                self.assertNotIn(RAW.encode(), candidate.read_bytes())

    def test_action_intent_is_written_before_dispatch(self) -> None:
        plugin = StubPlugin()
        original = self.store.append_event_with_checkpoint
        seen: list[dict[str, object]] = []
        def capture(run_id: str, event_type: str, payload: object, checkpoint: object, **kwargs: object) -> object:
            if event_type == "run.action_intent":
                seen.append(deepcopy(checkpoint))
                raise RuntimeError("crash-before-dispatch")
            return original(run_id, event_type, payload, checkpoint, **kwargs)
        with mock.patch.object(self.store, "append_event_with_checkpoint", side_effect=capture), self.assertRaises(AutomationError):
            self.executor().start(plan(), inputs={"query": "public"}, run_id="crash", plugins={"fixture": plugin})
        self.assertEqual(plugin.calls, [])
        intent = seen[0]["actionIntent"]
        self.assertEqual(set(intent), {"version", "operationId", "stepId", "reservationOrdinal", "attempt", "dispatchDeadlineEpochMs", "providerDigest", "contractDigest", "projectionDigest", "bindingDigest"})

    def _seed_action_intent(
        self, run_id: str, plugin: StubPlugin
    ) -> tuple[object, dict[str, object]]:
        workflow = plan()
        executor = self.executor()
        runner = executor._new_runner(
            workflow, plugins={"fixture": plugin}, allow_scripts=False,
            granted_permissions=None,
        )
        state = runner.initialize({"query": "public"})
        created = executor.service.create(
            run_id=run_id, workflow_name=workflow.name,
            workflow_version="1.0.0",
            plan_digest=__import__("ai_auto_desktop.runtime", fromlist=["canonical_plan_digest"]).canonical_plan_digest(workflow),
            inputs={"query": "public"}, descriptor=workflow,
        )
        lease = self.store.claim_owner(
            run_id, owner_id="dead", ttl_seconds=1, now=1
        )
        self.store.set_status_with_event(
            run_id, expected=RunStatus.PENDING, status=RunStatus.RUNNING,
            owner_id=lease.owner_id, token=lease.token, now=1.1,
            event_type="run.started", event_payload={},
            checkpoint=executor._checkpoint(state),
        )
        runner.prepare_segment()
        prepared = executor._prepare_action(runner, workflow.steps[0])
        reserved = runner.reserve_prepared_action_attempt()
        checkpoint = executor._action_intent_checkpoint(reserved, prepared)
        self.store.append_event_with_checkpoint(
            run_id, "run.action_intent", {}, checkpoint,
            owner_id=lease.owner_id, token=lease.token, now=1.2,
            expected_status=RunStatus.RUNNING,
        )
        runner.close()
        return workflow, checkpoint

    def test_action_intent_resume_replays_once_with_original_deadline(self) -> None:
        plugin = StubPlugin()
        workflow, checkpoint = self._seed_action_intent("replay", plugin)
        original_deadline = checkpoint["actionIntent"]["dispatchDeadlineEpochMs"]
        outcome = DurableExecutor(
            self.store, owner_id="recovery", durable_action_mode="read-only"
        ).resume("replay", workflow, plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(len(plugin.calls), 1)
        authorized = [e for e in self.store.list_events("replay") if e.event_type == "run.action_dispatch_authorized"]
        self.assertEqual(len(authorized), 1)
        self.assertEqual(checkpoint["actionIntent"]["dispatchDeadlineEpochMs"], original_deadline)

    def test_malformed_or_changed_intent_never_dispatches(self) -> None:
        for mutation in (
            lambda intent: intent.__setitem__("extra", True),
            lambda intent: intent.__setitem__("reservationOrdinal", 99),
            lambda intent: intent.__setitem__("projectionDigest", "sha256:" + "0" * 64),
        ):
            with self.subTest(mutation=mutation), tempfile.TemporaryDirectory() as temporary:
                path = Path(temporary) / "runs.sqlite3"
                with JournalStore(path) as store:
                    plugin = StubPlugin()
                    self.store = store
                    workflow, checkpoint = self._seed_action_intent("bad", plugin)
                    mutation(checkpoint["actionIntent"])
                    store._connection.execute(
                        "UPDATE runs SET checkpoint_json=? WHERE run_id=?",
                        (json.dumps(checkpoint), "bad"),
                    )
                    with self.assertRaises(AutomationError):
                        DurableExecutor(store, owner_id="recovery", durable_action_mode="read-only").resume("bad", workflow, plugins={"fixture": plugin})
                    self.assertEqual(plugin.calls, [])

    def test_cancelled_intent_is_not_dispatched(self) -> None:
        plugin = StubPlugin()
        workflow, _ = self._seed_action_intent("cancelled", plugin)
        from ai_auto_desktop.run_service import RunService
        RunService(self.store).request_cancel("cancelled")
        outcome = DurableExecutor(
            self.store, owner_id="recovery", durable_action_mode="read-only"
        ).resume("cancelled", workflow, plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.CANCELLED)
        self.assertEqual(plugin.calls, [])

    def test_expired_intent_times_out_without_dispatch(self) -> None:
        plugin = StubPlugin()
        workflow, checkpoint = self._seed_action_intent("expired", plugin)
        checkpoint["actionIntent"]["dispatchDeadlineEpochMs"] = 1
        self.store._connection.execute(
            "UPDATE runs SET checkpoint_json=? WHERE run_id=?",
            (json.dumps(checkpoint), "expired"),
        )
        outcome = DurableExecutor(
            self.store, owner_id="recovery", durable_action_mode="read-only"
        ).resume("expired", workflow, plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.TIMED_OUT)
        self.assertEqual(plugin.calls, [])

    def test_pause_race_at_replay_authorization_never_dispatches(self) -> None:
        plugin = StubPlugin()
        workflow, _ = self._seed_action_intent("pause-race", plugin)
        original = self.store.append_event_with_checkpoint

        def pause_before_authorize(
            run_id: str, event_type: str, payload: object,
            checkpoint: object, **kwargs: object,
        ) -> object:
            if event_type == "run.action_dispatch_authorized":
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_pause(run_id)
            return original(run_id, event_type, payload, checkpoint, **kwargs)

        with mock.patch.object(
            self.store, "append_event_with_checkpoint",
            side_effect=pause_before_authorize,
        ):
            outcome = DurableExecutor(
                self.store, owner_id="recovery",
                durable_action_mode="read-only",
            ).resume("pause-race", workflow, plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.PAUSED)
        self.assertEqual(outcome.run.desired_state, DesiredState.PAUSE)
        self.assertEqual(plugin.calls, [])

    def test_cancel_race_at_replay_authorization_never_dispatches(self) -> None:
        plugin = StubPlugin()
        workflow, _ = self._seed_action_intent("cancel-race", plugin)
        original = self.store.append_event_with_checkpoint

        def cancel_before_authorize(
            run_id: str, event_type: str, payload: object,
            checkpoint: object, **kwargs: object,
        ) -> object:
            if event_type == "run.action_dispatch_authorized":
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_cancel(run_id)
            return original(run_id, event_type, payload, checkpoint, **kwargs)

        with mock.patch.object(
            self.store, "append_event_with_checkpoint",
            side_effect=cancel_before_authorize,
        ):
            outcome = DurableExecutor(
                self.store, owner_id="recovery",
                durable_action_mode="read-only",
            ).resume("cancel-race", workflow, plugins={"fixture": plugin})
        self.assertEqual(outcome.run.status, RunStatus.CANCELLED)
        self.assertEqual(plugin.calls, [])

    def test_cancel_race_at_fresh_intent_never_dispatches(self) -> None:
        plugin = StubPlugin()
        original = self.store.append_event_with_checkpoint

        def cancel_before_intent(
            run_id: str, event_type: str, payload: object,
            checkpoint: object, **kwargs: object,
        ) -> object:
            if event_type == "run.action_intent":
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_cancel(run_id)
            return original(run_id, event_type, payload, checkpoint, **kwargs)

        with mock.patch.object(
            self.store, "append_event_with_checkpoint",
            side_effect=cancel_before_intent,
        ):
            outcome = self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="fresh-cancel", plugins={"fixture": plugin},
            )
        self.assertEqual(outcome.run.status, RunStatus.CANCELLED)
        self.assertEqual(plugin.calls, [])

    def test_pause_race_at_fresh_intent_never_dispatches(self) -> None:
        plugin = StubPlugin()
        original = self.store.append_event_with_checkpoint

        def pause_before_intent(
            run_id: str, event_type: str, payload: object,
            checkpoint: object, **kwargs: object,
        ) -> object:
            if event_type == "run.action_intent":
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_pause(run_id)
            return original(run_id, event_type, payload, checkpoint, **kwargs)

        with mock.patch.object(
            self.store, "append_event_with_checkpoint",
            side_effect=pause_before_intent,
        ):
            outcome = self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="fresh-pause", plugins={"fixture": plugin},
            )
        self.assertEqual(outcome.run.status, RunStatus.PAUSED)
        self.assertEqual(plugin.calls, [])

    def test_cancel_race_at_terminal_commit_returns_only_cancelled_state(self) -> None:
        plugin = StubPlugin()
        original = self.store.set_status_with_event
        raced = False

        def cancel_before_finish(run_id: str, **kwargs: object) -> object:
            nonlocal raced
            if kwargs.get("event_type") == "run.finished" and not raced:
                raced = True
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_cancel(run_id)
            return original(run_id, **kwargs)

        with mock.patch.object(
            self.store, "set_status_with_event",
            side_effect=cancel_before_finish,
        ):
            outcome = self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="finish-cancel", plugins={"fixture": plugin},
            )

        self.assertEqual(outcome.run.status, RunStatus.CANCELLED)
        self.assertEqual(outcome.run.desired_state, DesiredState.CANCEL)
        self.assertIsNone(outcome.result)
        event_types = [
            event.event_type for event in self.store.list_events("finish-cancel")
        ]
        self.assertIn("run.cancel_requested", event_types)
        self.assertIn("run.cancelled", event_types)
        self.assertNotIn("run.finished", event_types)

    def test_pause_race_at_terminal_commit_finishes_instead_of_stranding(self) -> None:
        plugin = StubPlugin()
        original = self.store.set_status_with_event
        raced = False

        def pause_before_finish(run_id: str, **kwargs: object) -> object:
            nonlocal raced
            if kwargs.get("event_type") == "run.finished" and not raced:
                raced = True
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_pause(run_id)
            return original(run_id, **kwargs)

        with mock.patch.object(
            self.store, "set_status_with_event",
            side_effect=pause_before_finish,
        ):
            outcome = self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="finish-pause", plugins={"fixture": plugin},
            )

        self.assertEqual(outcome.run.status, RunStatus.SUCCEEDED)
        self.assertEqual(outcome.run.desired_state, DesiredState.PAUSE)
        self.assertEqual(outcome.result.status, "succeeded")
        self.assertEqual(outcome.run.output, {"title": "ok"})
        event_types = [
            event.event_type for event in self.store.list_events("finish-pause")
        ]
        self.assertIn("run.pause_requested", event_types)
        self.assertEqual(event_types[-1], "run.finished")

    def test_cancel_does_not_downgrade_unknown_effect_at_terminal_commit(self) -> None:
        plugin = StubPlugin(PluginError("FIXTURE.UNDECLARED", RAW))
        original = self.store.set_status_with_event
        raced = False

        def cancel_before_finish(run_id: str, **kwargs: object) -> object:
            nonlocal raced
            if kwargs.get("event_type") == "run.finished" and not raced:
                raced = True
                from ai_auto_desktop.run_service import RunService
                RunService(self.store).request_cancel(run_id)
            return original(run_id, **kwargs)

        with mock.patch.object(
            self.store, "set_status_with_event",
            side_effect=cancel_before_finish,
        ):
            outcome = self.executor().start(
                plan(), inputs={"query": "public"},
                run_id="unknown-cancel", plugins={"fixture": plugin},
            )

        self.assertEqual(outcome.run.status, RunStatus.UNKNOWN_EFFECT)
        self.assertEqual(outcome.run.desired_state, DesiredState.CANCEL)
        self.assertEqual(outcome.result.status, "unknown_effect")
        self.assertEqual(outcome.run.error["code"], "ACTION.UNDECLARED_ERROR")

    def test_v1_boundary_resumes_and_v1_in_step_remains_unknown(self) -> None:
        from ai_auto_desktop.runtime import WorkflowRunner, canonical_plan_digest
        simple = compile_descriptor({
            "apiVersion": "ai-auto-desktop.dev/v1alpha1",
            "kind": "Workflow", "metadata": {"name": "v1"},
            "budgets": {"max_duration": "5s", "max_executed_steps": 2},
            "steps": [{"id": "done", "type": "return", "value": 1}],
        })
        for name, phase, expected in (
            ("v1-safe", "between_top_level_steps", RunStatus.SUCCEEDED),
            ("v1-unsafe", "in_top_level_step", RunStatus.UNKNOWN_EFFECT),
        ):
            self.store.create_run_with_event(
                run_id=name, workflow_name=simple.name, workflow_version=None,
                plan_digest=canonical_plan_digest(simple), inputs={},
                descriptor=simple, event_type="run.created", event_payload={},
            )
            lease = self.store.claim_owner(name, owner_id="dead", ttl_seconds=1, now=1)
            runner = WorkflowRunner(simple); state = runner.initialize()
            if phase == "in_top_level_step": state = runner.prepare_segment().state
            self.store.set_status_with_event(
                name, expected=RunStatus.PENDING, status=RunStatus.RUNNING,
                owner_id=lease.owner_id, token=lease.token, now=1.1,
                event_type="run.started", event_payload={},
                checkpoint={"checkpointSchemaVersion": 1, **state.to_dict()},
            )
            outcome = DurableExecutor(
                self.store, owner_id="recovery",
                durable_action_mode="read-only",
            ).resume(name, simple)
            self.assertEqual(outcome.run.status, expected)

    def test_cli_flag_is_explicit(self) -> None:
        options = build_parser().parse_args(["start", "flow.json", "--journal", "runs.db", "--durable-actions", "read-only"])
        self.assertEqual(options.durable_actions, "read-only")


if __name__ == "__main__":
    unittest.main()
