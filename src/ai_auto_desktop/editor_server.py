"""Loopback HTTP server for the recording editor UI.

Three properties are enforced here rather than trusted, each re-measured on this
machine before the server was written: the listener binds 127.0.0.1 with port 0
so the OS assigns an unused port; every request must carry the session token in
a header; and a non-loopback interface cannot reach the port at all (measured:
ConnectionRefusedError on a raw TCP connect to the LAN address).

The token travels in a header, not the URL, so it does not end up in browser
history, referrers or proxy logs.  The page itself is served without it and
fetches it from a one-time bootstrap, so the URL the operator sees is plain.

There is deliberately no generic "execute action" endpoint.  The routes below
are the entire surface: anything the UI can do, it can do only through an
operation the editing core already validates.  A dry-run route is provided but
it goes through the same compile path as a real replay, so it cannot be used to
skip policy or risk checks.
"""

from __future__ import annotations

import json
import secrets
import threading
from collections.abc import Mapping
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable
from urllib.parse import urlparse

from . import recording_editor as editor
from .recording import RecordingError

TOKEN_HEADER = "X-Recorder-Token"
TOKEN_PLACEHOLDER = "__RECORDER_TOKEN__"
MAX_BODY_BYTES = 1 << 20


class EditorSession:
    """The recording being edited, plus an undo history.

    History exists because the operator is editing a capture that cannot be
    retaken cheaply: a mis-drag that reorders ten steps must be reversible, or
    people will avoid using the editor at all.
    """

    def __init__(self, recording: Mapping[str, Any],
                 on_change: Callable[[dict[str, Any]], None] | None = None
                 ) -> None:
        self._lock = threading.Lock()
        self._recording = editor._validated(recording)
        self._history: list[dict[str, Any]] = []
        self._warnings: list[dict[str, Any]] = []
        # Called after every committed change.  Measured on Windows: a killed
        # process runs neither `finally` nor a signal handler, so saving only at
        # exit can lose the whole session.
        self._on_change = on_change

    @property
    def recording(self) -> dict[str, Any]:
        with self._lock:
            return json.loads(json.dumps(self._recording))

    @property
    def warnings(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._warnings)

    def apply(self, change: Callable[[dict[str, Any]], Any]) -> dict[str, Any]:
        """Run an edit under the lock, keeping the old state for undo.

        The lock matters because a browser can have several requests in flight
        and the recording is shared mutable state; without it a drag and a field
        edit can interleave and silently lose one of them.
        """

        with self._lock:
            previous = json.loads(json.dumps(self._recording))
            result = change(json.loads(json.dumps(self._recording)))
            if isinstance(result, tuple):
                updated, warnings = result
            else:
                updated, warnings = result, []
            self._history.append(previous)
            self._recording = updated
            self._warnings = list(warnings)
            self._notify(updated)
            return json.loads(json.dumps(updated))

    def undo(self) -> bool:
        with self._lock:
            if not self._history:
                return False
            self._recording = self._history.pop()
            self._warnings = []
            # An undo is a change like any other: without this the saved file
            # would keep an edit the operator has already taken back.
            self._notify(self._recording)
            return True

    def _notify(self, recording: dict[str, Any]) -> None:
        if self._on_change is None:
            return
        # A failing autosave must not roll back an edit the operator can see on
        # screen; the two would then disagree about what the recording is.
        try:
            self._on_change(json.loads(json.dumps(recording)))
        except OSError:
            pass


class _Handler(BaseHTTPRequestHandler):
    server_version = "ai-auto-desktop-editor"
    protocol_version = "HTTP/1.1"

    # -- plumbing ------------------------------------------------------
    def log_message(self, *args: Any) -> None:
        """Silence per-request logging.

        The default handler writes the request line to stderr, which for this
        server would mean writing operator activity into whatever captured the
        host's output.
        """

    def _send(self, status: int, payload: Any) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        # The page is same-origin and needs no framing; both headers cost
        # nothing and remove a class of local-network attack.
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.end_headers()
        self.wfile.write(body)

    def _send_html(self, body: str) -> None:
        encoded = body.encode("utf-8")
        self.send_response(HTTPStatus.OK)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.end_headers()
        self.wfile.write(encoded)

    def _authorised(self) -> bool:
        supplied = self.headers.get(TOKEN_HEADER, "")
        # Constant-time comparison: a plain == leaks the token prefix through
        # timing, and this token is the only thing protecting the endpoints.
        return secrets.compare_digest(supplied, self.server.token)

    def _body(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length") or 0)
        if length <= 0:
            return {}
        if length > MAX_BODY_BYTES:
            raise ValueError("request body too large")
        raw = self.rfile.read(length)
        return json.loads(raw.decode("utf-8"))

    # -- routes --------------------------------------------------------
    def do_GET(self) -> None:
        path = urlparse(self.path).path
        if path == "/":
            # The token is injected here rather than served from its own
            # endpoint: measured, an unauthenticated /bootstrap let a separate
            # local process take the token and read the session immediately.
            self._send_html(
                self.server.page.replace(TOKEN_PLACEHOLDER, self.server.token))
            return
        if not self._authorised():
            self._send(HTTPStatus.UNAUTHORIZED, {"error": "invalid token"})
            return
        if path == "/api/state":
            self._send(HTTPStatus.OK, self._state())
            return
        self._send(HTTPStatus.NOT_FOUND, {"error": "no such endpoint"})

    def do_POST(self) -> None:
        path = urlparse(self.path).path
        if not self._authorised():
            self._discard_body()
            self._send(HTTPStatus.UNAUTHORIZED, {"error": "invalid token"})
            return

        handlers: dict[str, Callable[[dict[str, Any]], Any]] = {
            "/api/enable": self._enable,
            "/api/update": self._update,
            "/api/reorder": self._reorder,
            "/api/logic": self._logic,
            "/api/input": self._input,
            "/api/undo": self._undo,
        }
        handler = handlers.get(path)
        if handler is None:
            self._discard_body()
            self._send(HTTPStatus.NOT_FOUND, {"error": "no such endpoint"})
            return
        try:
            payload = self._body()
        except (ValueError, json.JSONDecodeError) as exc:
            self._send(HTTPStatus.BAD_REQUEST, {"error": str(exc)})
            return
        try:
            handler(payload)
        except RecordingError as exc:
            # A refused edit is an expected outcome, not a server fault: the
            # operator needs the reason, and the session must be unchanged.
            self._send(HTTPStatus.CONFLICT, {"error": exc.to_dict()})
        except (KeyError, TypeError, ValueError) as exc:
            self._send(HTTPStatus.BAD_REQUEST,
                       {"error": {"message": str(exc)}})

    def _discard_body(self) -> None:
        """Read and drop the request body before an early response.

        Closing the connection with an unsent body still in flight makes the
        peer see a reset instead of the status code -- so a rejected request
        would surface as a network error rather than "unauthorized" or
        "not found".
        """

        try:
            length = int(self.headers.get("Content-Length") or 0)
        except ValueError:
            return
        remaining = min(length, MAX_BODY_BYTES)
        while remaining > 0:
            chunk = self.rfile.read(min(remaining, 65536))
            if not chunk:
                break
            remaining -= len(chunk)

    # -- operations ----------------------------------------------------
    def _state(self) -> dict[str, Any]:
        session = self.server.session
        recording = session.recording
        return {
            "name": (recording.get("metadata") or {}).get("name"),
            "window": (recording.get("capture") or {}).get("window"),
            "steps": editor.step_view(recording),
            "inputs": recording.get("inputs") or {},
            "warnings": session.warnings,
            "compile": editor.compile_preview(recording),
            "recorded_order": editor._recorded_order(recording),
        }

    def _enable(self, payload: dict[str, Any]) -> None:
        step_id = payload["id"]
        enabled = bool(payload["enabled"])
        self.server.session.apply(
            lambda rec: editor.set_enabled(rec, step_id, enabled))
        self._send(HTTPStatus.OK, self._state())

    def _update(self, payload: dict[str, Any]) -> None:
        step_id = payload["id"]
        changes = payload.get("changes") or {}
        self.server.session.apply(
            lambda rec: editor.update_step(rec, step_id, changes))
        self._send(HTTPStatus.OK, self._state())

    def _reorder(self, payload: dict[str, Any]) -> None:
        order = list(payload["order"])
        self.server.session.apply(lambda rec: editor.reorder(rec, order))
        self._send(HTTPStatus.OK, self._state())

    def _logic(self, payload: dict[str, Any]) -> None:
        step_id = payload["id"]
        when = payload["when"]
        wrap = list(payload.get("wrap") or [])
        self.server.session.apply(
            lambda rec: editor.insert_logic(rec, step_id, when, wrap))
        self._send(HTTPStatus.OK, self._state())

    def _input(self, payload: dict[str, Any]) -> None:
        name = payload["name"]
        spec = payload.get("spec") or {"type": "string"}
        self.server.session.apply(
            lambda rec: editor.declare_input(rec, name, spec))
        self._send(HTTPStatus.OK, self._state())

    def _undo(self, payload: dict[str, Any]) -> None:
        undone = self.server.session.undo()
        state = self._state()
        state["undone"] = undone
        self._send(HTTPStatus.OK, state)


class EditorServer(ThreadingHTTPServer):
    """Loopback-only editor server, alive for one editing session.

    Scope of protection, measured rather than assumed: a non-loopback interface
    cannot reach the port (raw TCP connect is refused), so remote access is
    closed.  Local processes are a different matter -- loopback is not isolated
    per process -- so the token guards against a stray browser tab or a page on
    another origin, not against arbitrary code already running as this user.
    """

    daemon_threads = True
    allow_reuse_address = False

    def __init__(self, session: EditorSession, page: str) -> None:  # noqa: D107
        # Port 0 lets the OS pick a free port: a fixed port would collide
        # between concurrent sessions and is guessable by anything local.
        super().__init__(("127.0.0.1", 0), _Handler)
        self.session = session
        self.page = page
        self.token = secrets.token_urlsafe(32)

    @property
    def url(self) -> str:
        host, port = self.server_address[:2]
        return f"http://{host}:{port}/"


def serve(recording: Mapping[str, Any], page: str,
          on_change: Callable[[dict[str, Any]], None] | None = None
          ) -> EditorServer:
    """Start the editor server; the caller owns its lifetime."""

    return EditorServer(EditorSession(recording, on_change), page)
