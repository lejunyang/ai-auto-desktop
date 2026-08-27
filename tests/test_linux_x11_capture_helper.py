"""X11 target capture helper 的隔离 Xvfb 集成测试。

Linux 上缺少编译器、X11 开发包或 Xvfb 是测试环境错误，不能用 skip 冒充通过。
非 Linux 平台则不具备被测实现的运行资格。
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import platform
import select
import shlex
import shutil
import struct
import subprocess
import tempfile
import time
import unittest
import zlib


PROJECT_ROOT = Path(__file__).resolve().parents[1]
BUILD_SCRIPT = (
    PROJECT_ROOT / "plugins" / "linux_atspi" / "build_x11_capture_helper.sh"
)
PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
CAPTURE = (60, 50, 100, 80)

FIXTURE_SOURCE = r"""
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/cursorfont.h>

#include <cerrno>
#include <cstdio>
#include <cstring>
#include <string_view>
#include <unistd.h>

namespace {

unsigned long NamedColor(Display *display, const char *name) {
    XColor exact{};
    XColor screen{};
    if (XAllocNamedColor(display, DefaultColormap(display, DefaultScreen(display)),
                         name, &screen, &exact) == 0) {
        std::fprintf(stderr, "XAllocNamedColor failed: %s\n", name);
        return BlackPixel(display, DefaultScreen(display));
    }
    return screen.pixel;
}

Window MakeWindow(Display *display, Window root, Atom pid_atom, int x, int y,
                  unsigned int width, unsigned int height, const char *color) {
    const Window window = XCreateSimpleWindow(
        display, root, x, y, width, height, 0,
        BlackPixel(display, DefaultScreen(display)), NamedColor(display, color));
    const unsigned long pid = static_cast<unsigned long>(getpid());
    XChangeProperty(display, window, pid_atom, XA_CARDINAL, 32, PropModeReplace,
                    reinterpret_cast<const unsigned char *>(&pid), 1);
    XMapRaised(display, window);
    XClearWindow(display, window);
    return window;
}

Window MakeFramedWindow(Display *display, Window root, Atom pid_atom) {
    const Window frame = XCreateSimpleWindow(
        display, root, 30, 20, 220, 180, 0,
        BlackPixel(display, DefaultScreen(display)), NamedColor(display, "#444444"));
    const Window client = XCreateSimpleWindow(
        display, frame, 10, 10, 200, 160, 0,
        BlackPixel(display, DefaultScreen(display)), NamedColor(display, "#ff0000"));
    const unsigned long pid = static_cast<unsigned long>(getpid());
    XChangeProperty(display, client, pid_atom, XA_CARDINAL, 32, PropModeReplace,
                    reinterpret_cast<const unsigned char *>(&pid), 1);
    XMapWindow(display, client);
    XMapRaised(display, frame);
    XClearWindow(display, client);
    return client;
}

Window MakeChildOverlayWindow(Display *display, Window root, Atom pid_atom,
                              Window *overlay) {
    const Window client = MakeWindow(
        display, root, pid_atom, 40, 30, 200, 160, "#ff0000");
    *overlay = XCreateSimpleWindow(
        display, client, 20, 20, 20, 20, 0,
        BlackPixel(display, DefaultScreen(display)), NamedColor(display, "#00ff00"));
    XMapRaised(display, *overlay);
    XClearWindow(display, *overlay);
    return client;
}

}  // namespace

int main(int argc, char **argv) {
    if (argc != 2) {
        std::fprintf(
            stderr,
            "usage: fixture target|framed|child-overlay|sibling|overlay\n");
        return 64;
    }
    Display *display = XOpenDisplay(nullptr);
    if (display == nullptr) {
        std::fprintf(stderr, "XOpenDisplay failed\n");
        return 69;
    }
    const std::string_view mode(argv[1]);
    const Window root = DefaultRootWindow(display);
    const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
    Window primary = None;
    Window sibling = None;
    if (mode == "target" || mode == "sibling") {
        primary = MakeWindow(display, root, pid_atom, 40, 30, 200, 160, "#ff0000");
        const Cursor cursor = XCreateFontCursor(display, XC_crosshair);
        XDefineCursor(display, primary, cursor);
        XWarpPointer(display, None, primary, 0, 0, 0, 0, 70, 60);
        if (mode == "sibling") {
            sibling =
                MakeWindow(display, root, pid_atom, 60, 50, 20, 20, "#00ff00");
        }
    } else if (mode == "framed") {
        primary = MakeFramedWindow(display, root, pid_atom);
    } else if (mode == "child-overlay") {
        primary = MakeChildOverlayWindow(display, root, pid_atom, &sibling);
    } else if (mode == "overlay") {
        primary = MakeWindow(display, root, pid_atom, 60, 50, 20, 20, "#0000ff");
    } else {
        std::fprintf(stderr, "unknown mode\n");
        XCloseDisplay(display);
        return 64;
    }
    XSync(display, False);
    std::printf("READY %lu %lu\n", primary, sibling);
    std::fflush(stdout);
    char buffer[32];
    while (true) {
        const ssize_t count = read(STDIN_FILENO, buffer, sizeof(buffer));
        if (count == 0) {
            break;
        }
        if (count < 0 && errno != EINTR) {
            break;
        }
    }
    XCloseDisplay(display);
    return 0;
}
"""


def _require_linux_dependency(command: str) -> str:
    resolved = shutil.which(command)
    if resolved is None:
        raise AssertionError(
            f"Linux X11 capture tests require {command}; missing dependency is a failure"
        )
    return resolved


def _decode_rgba_png(data: bytes) -> tuple[int, int, bytes]:
    """Decode the deliberately small PNG subset emitted by the helper."""

    if not data.startswith(PNG_SIGNATURE):
        raise AssertionError("capture stdout is not a PNG")
    offset = len(PNG_SIGNATURE)
    width = height = None
    compressed = bytearray()
    saw_end = False
    while offset < len(data):
        if len(data) - offset < 12:
            raise AssertionError("truncated PNG chunk")
        length = struct.unpack(">I", data[offset : offset + 4])[0]
        chunk_type = data[offset + 4 : offset + 8]
        payload_start = offset + 8
        payload_end = payload_start + length
        crc_end = payload_end + 4
        if crc_end > len(data):
            raise AssertionError("PNG chunk exceeds stdout")
        payload = data[payload_start:payload_end]
        expected_crc = struct.unpack(">I", data[payload_end:crc_end])[0]
        if zlib.crc32(chunk_type + payload) & 0xFFFFFFFF != expected_crc:
            raise AssertionError(f"bad PNG CRC for {chunk_type!r}")
        if chunk_type == b"IHDR":
            if length != 13:
                raise AssertionError("invalid IHDR length")
            width, height, depth, color_type, compression, filtering, interlace = (
                struct.unpack(">IIBBBBB", payload)
            )
            if (depth, color_type, compression, filtering, interlace) != (
                8,
                6,
                0,
                0,
                0,
            ):
                raise AssertionError("unexpected PNG encoding")
        elif chunk_type == b"IDAT":
            compressed.extend(payload)
        elif chunk_type == b"IEND":
            if length != 0:
                raise AssertionError("invalid IEND")
            saw_end = True
            offset = crc_end
            break
        offset = crc_end
    if width is None or height is None or not compressed or not saw_end:
        raise AssertionError("incomplete PNG")
    if offset != len(data):
        raise AssertionError("bytes follow PNG IEND")
    raw = zlib.decompress(bytes(compressed))
    row_size = 1 + width * 4
    if len(raw) != row_size * height:
        raise AssertionError("unexpected decompressed PNG size")
    pixels = bytearray(width * height * 4)
    for row in range(height):
        source = row * row_size
        if raw[source] != 0:
            raise AssertionError("helper must emit deterministic filter type 0")
        destination = row * width * 4
        pixels[destination : destination + width * 4] = raw[
            source + 1 : source + row_size
        ]
    return width, height, bytes(pixels)


@unittest.skipUnless(platform.system() == "Linux", "X11 helper only runs on Linux")
class LinuxX11CaptureHelperTests(unittest.TestCase):
    temporary: tempfile.TemporaryDirectory[str]
    helper: Path
    fixture: Path
    xvfb: subprocess.Popen[bytes]
    environment: dict[str, str]

    @classmethod
    def setUpClass(cls) -> None:
        super().setUpClass()
        gxx = _require_linux_dependency("g++")
        pkg_config = _require_linux_dependency("pkg-config")
        xvfb = _require_linux_dependency("Xvfb")
        x11_probe = subprocess.run(
            [pkg_config, "--exists", "x11"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=10,
        )
        if x11_probe.returncode != 0:
            raise AssertionError(
                "Linux X11 capture tests require the X11 development package; "
                + x11_probe.stderr.decode("utf-8", errors="replace")[-1000:]
            )

        cls.temporary = tempfile.TemporaryDirectory(prefix="aad-x11-capture-")
        temporary_path = Path(cls.temporary.name)
        build_environment = os.environ.copy()
        build_environment["AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR"] = str(
            temporary_path / "build"
        )
        build = subprocess.run(
            ["sh", str(BUILD_SCRIPT)],
            env=build_environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        if build.returncode != 0:
            cls.temporary.cleanup()
            raise AssertionError(
                "capture helper build failed:\n"
                + build.stderr.decode("utf-8", errors="replace")[-4000:]
            )
        cls.helper = temporary_path / "build" / "x11_capture_helper"
        if not cls.helper.is_file() or not os.access(cls.helper, os.X_OK):
            cls.temporary.cleanup()
            raise AssertionError("build script did not create executable x11_capture_helper")

        fixture_source = temporary_path / "x11_capture_fixture.cpp"
        fixture_source.write_text(FIXTURE_SOURCE, encoding="utf-8")
        cls.fixture = temporary_path / "x11_capture_fixture"
        flags = subprocess.run(
            [pkg_config, "--cflags", "--libs", "x11"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=10,
        )
        if flags.returncode != 0:
            cls.temporary.cleanup()
            raise AssertionError(
                "pkg-config x11 flags failed: "
                + flags.stderr.decode("utf-8", errors="replace")[-1000:]
            )
        fixture_build = subprocess.run(
            [
                gxx,
                "-std=c++17",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                str(fixture_source),
                "-o",
                str(cls.fixture),
                *shlex.split(flags.stdout.decode("utf-8", errors="strict")),
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        if fixture_build.returncode != 0:
            cls.temporary.cleanup()
            raise AssertionError(
                "X11 test fixture build failed:\n"
                + fixture_build.stderr.decode("utf-8", errors="replace")[-4000:]
            )

        cls.xvfb = subprocess.Popen(
            [
                xvfb,
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-nolisten",
                "tcp",
                "-noreset",
                "-ac",
            ],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        try:
            assert cls.xvfb.stdout is not None
            readable, _, _ = select.select([cls.xvfb.stdout], [], [], 5)
            if not readable:
                raise AssertionError("Xvfb did not publish a display within 5 seconds")
            display_number = (
                cls.xvfb.stdout.readline(64)
                .decode("ascii", errors="strict")
                .strip()
            )
            if not display_number.isdigit():
                raise AssertionError(f"invalid Xvfb display number: {display_number!r}")
        except BaseException:
            cls._stop_xvfb()
            cls.temporary.cleanup()
            raise
        cls.environment = os.environ.copy()
        cls.environment.update(
            {
                "DISPLAY": f":{display_number}",
                "XDG_SESSION_TYPE": "x11",
                "XDG_CURRENT_DESKTOP": "KDE",
            }
        )
        ready_deadline = time.monotonic() + 5
        last_probe = b""
        while time.monotonic() < ready_deadline:
            if cls.xvfb.poll() is not None:
                assert cls.xvfb.stderr is not None
                diagnostics = cls.xvfb.stderr.read(2000)
                cls._stop_xvfb()
                cls.temporary.cleanup()
                raise AssertionError(
                    f"Xvfb exited before accepting X11 clients: {diagnostics!r}"
                )
            probe = subprocess.run(
                [str(cls.fixture), "target"],
                env=cls.environment,
                input=b"",
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=2,
            )
            last_probe = probe.stderr
            if probe.returncode == 0 and probe.stdout.startswith(b"READY "):
                break
            time.sleep(0.02)
        else:
            cls._stop_xvfb()
            cls.temporary.cleanup()
            raise AssertionError(
                "Xvfb did not accept an Xlib client within 5 seconds: "
                + last_probe.decode("utf-8", errors="replace")[-1000:]
            )

    @classmethod
    def tearDownClass(cls) -> None:
        cls._stop_xvfb()
        if hasattr(cls, "temporary"):
            cls.temporary.cleanup()
        super().tearDownClass()

    @classmethod
    def _stop_xvfb(cls) -> None:
        process = getattr(cls, "xvfb", None)
        if process is None:
            return
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()

    def setUp(self) -> None:
        self.fixtures: list[subprocess.Popen[bytes]] = []

    def tearDown(self) -> None:
        for process in reversed(self.fixtures):
            if process.stdin is not None and not process.stdin.closed:
                process.stdin.close()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
            if process.stdout is not None:
                process.stdout.close()
            if process.stderr is not None:
                process.stderr.close()

    def _start_fixture(self, mode: str) -> subprocess.Popen[bytes]:
        process = subprocess.Popen(
            [str(self.fixture), mode],
            env=self.environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self.fixtures.append(process)
        assert process.stdout is not None
        readable, _, _ = select.select([process.stdout], [], [], 5)
        if not readable:
            self.fail(f"X11 fixture {mode!r} did not become ready")
        ready = process.stdout.readline()
        if not ready.startswith(b"READY "):
            assert process.stderr is not None
            self.fail(
                f"X11 fixture {mode!r} failed: {ready!r}; "
                f"{process.stderr.read(2000)!r}"
            )
        return process

    def _command(
        self,
        expected_pid: int,
        *,
        region: tuple[int, int, int, int] = CAPTURE,
        deadline_ns: int | None = None,
    ) -> list[str]:
        x, y, width, height = region
        return [
            str(self.helper),
            "capture-target",
            "--expected-pid",
            str(expected_pid),
            "--x",
            str(x),
            "--y",
            str(y),
            "--width",
            str(width),
            "--height",
            str(height),
            "--deadline-monotonic-ns",
            str(deadline_ns or time.monotonic_ns() + 5_000_000_000),
        ]

    def _run(
        self, command: list[str], *, environment: dict[str, str] | None = None
    ) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            command,
            env=self.environment if environment is None else environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=10,
        )

    def _assert_error(
        self,
        result: subprocess.CompletedProcess[bytes],
        *,
        exit_code: int,
        code: str,
    ) -> dict[str, object]:
        self.assertEqual(result.returncode, exit_code, result.stderr)
        self.assertEqual(result.stdout, b"")
        self.assertLessEqual(len(result.stderr), 2048)
        self.assertEqual(result.stderr.count(b"\n"), 1)
        metadata = json.loads(result.stderr)
        self.assertIs(metadata["ok"], False)
        self.assertEqual(metadata["schema_version"], 1)
        self.assertEqual(metadata["code"], code)
        return metadata

    def test_captures_visible_target_pixels_as_pathless_png_without_cursor(self) -> None:
        target = self._start_fixture("target")
        result = self._run(self._command(target.pid))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLessEqual(len(result.stderr), 2048)
        self.assertEqual(result.stderr.count(b"\n"), 1)
        metadata = json.loads(result.stderr)
        self.assertEqual(
            {
                "ok": metadata["ok"],
                "schema_version": metadata["schema_version"],
                "capture_method": metadata["capture_method"],
                "format": metadata["format"],
                "mime_type": metadata["mime_type"],
                "expected_pid": metadata["expected_pid"],
                "target_pid": metadata["target_pid"],
                "cursor_included": metadata["cursor_included"],
                "occlusion_checked": metadata["occlusion_checked"],
                "same_euid_verified": metadata["same_euid_verified"],
                "scene_stable": metadata["scene_stable"],
            },
            {
                "ok": True,
                "schema_version": 1,
                "capture_method": "x11_root_xgetimage",
                "format": "png",
                "mime_type": "image/png",
                "expected_pid": target.pid,
                "target_pid": target.pid,
                "cursor_included": False,
                "occlusion_checked": True,
                "same_euid_verified": True,
                "scene_stable": True,
            },
        )
        self.assertEqual(
            (metadata["x"], metadata["y"], metadata["width"], metadata["height"]),
            CAPTURE,
        )
        self.assertEqual(metadata["root_width"], 320)
        self.assertEqual(metadata["root_height"], 240)
        self.assertEqual(metadata["png_bytes"], len(result.stdout))
        self.assertGreater(metadata["target_window"], 0)
        self.assertGreater(metadata["target_top_level_window"], 0)
        self.assertGreater(metadata["root_window"], 0)

        width, height, pixels = _decode_rgba_png(result.stdout)
        self.assertEqual((width, height), CAPTURE[2:])
        expected = bytes((255, 0, 0, 255))
        # The fixture moves a crosshair cursor to (50, 40) in this capture.
        # Root XGetImage must still return the underlying red application pixel.
        cursor_offset = (40 * width + 50) * 4
        self.assertEqual(pixels[cursor_offset : cursor_offset + 4], expected)
        self.assertEqual(set(zip(*(iter(pixels),) * 4)), {(255, 0, 0, 255)})

    def test_rejects_external_and_same_pid_sibling_top_level_occlusion(self) -> None:
        target = self._start_fixture("target")
        overlay = self._start_fixture("overlay")
        self.assertNotEqual(target.pid, overlay.pid)
        external = self._run(self._command(target.pid))
        external_metadata = self._assert_error(
            external, exit_code=73, code="target_occluded"
        )
        self.assertEqual(external_metadata["phase"], "occlusion_preflight")

        # Close both X clients before constructing a same-process sibling scene.
        self.tearDown()
        self.setUp()
        sibling_target = self._start_fixture("sibling")
        sibling = self._run(self._command(sibling_target.pid))
        sibling_metadata = self._assert_error(
            sibling, exit_code=73, code="target_occluded"
        )
        self.assertEqual(sibling_metadata["phase"], "occlusion_preflight")

    def test_accepts_window_manager_frame_with_pid_on_descendant_client(self) -> None:
        framed = self._start_fixture("framed")
        result = self._run(self._command(framed.pid))
        self.assertEqual(result.returncode, 0, result.stderr)
        metadata = json.loads(result.stderr)
        self.assertEqual(metadata["target_pid"], framed.pid)
        self.assertNotEqual(
            metadata["target_window"], metadata["target_top_level_window"]
        )
        width, height, pixels = _decode_rgba_png(result.stdout)
        self.assertEqual((width, height), CAPTURE[2:])
        self.assertEqual(set(zip(*(iter(pixels),) * 4)), {(255, 0, 0, 255)})

    def test_rejects_same_top_level_child_overlay(self) -> None:
        target = self._start_fixture("child-overlay")
        result = self._run(self._command(target.pid))
        metadata = self._assert_error(
            result, exit_code=73, code="target_occluded"
        )
        self.assertEqual(metadata["phase"], "occlusion_preflight")

    def test_rejects_wrong_center_pid_and_root_overflow(self) -> None:
        target = self._start_fixture("target")
        wrong_pid = self._run(self._command(os.getpid()))
        self._assert_error(wrong_pid, exit_code=73, code="target_pid_mismatch")

        outside = self._run(
            self._command(target.pid, region=(300, 220, 30, 30))
        )
        metadata = self._assert_error(
            outside, exit_code=73, code="bounds_out_of_root"
        )
        self.assertEqual(metadata["phase"], "bounds_preflight")

    def test_rejects_invalid_argv_wayland_and_expired_deadline(self) -> None:
        invalid = self._run([str(self.helper), "capture-target"])
        self._assert_error(invalid, exit_code=64, code="invalid_arguments")
        oversized = self._run(
            [
                str(self.helper), "capture-target", "--expected-pid", "1",
                "--x", "0", "--y", "0", "--width", "4096",
                "--height", "4096", "--deadline-monotonic-ns",
                str(time.monotonic_ns() + 5_000_000_000),
            ]
        )
        self._assert_error(oversized, exit_code=64, code="invalid_arguments")

        target = self._start_fixture("target")
        wayland_environment = dict(self.environment)
        wayland_environment["XDG_SESSION_TYPE"] = "wayland"
        wayland = self._run(
            self._command(target.pid), environment=wayland_environment
        )
        self._assert_error(wayland, exit_code=69, code="unsupported_session")

        expired = self._run(
            self._command(target.pid, deadline_ns=time.monotonic_ns() - 1)
        )
        self._assert_error(expired, exit_code=75, code="deadline_exceeded")

    def test_build_rejects_symlink_build_directory(self) -> None:
        target = Path(self.temporary.name) / "symlink-build-target"
        target.mkdir(mode=0o700)
        link = Path(self.temporary.name) / "symlink-build"
        link.symlink_to(target, target_is_directory=True)
        environment = os.environ.copy()
        environment["AI_AUTO_DESKTOP_LINUX_ATSPI_BUILD_DIR"] = str(link)
        build = subprocess.run(
            ["sh", str(BUILD_SCRIPT)],
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=30,
        )
        self.assertEqual(build.returncode, 72, build.stderr)
        self.assertFalse((target / "x11_capture_helper").exists())

    def test_rejects_nonexistent_process_before_opening_display_target(self) -> None:
        # PID_MAX_LIMIT on Linux is below INT_MAX, so this is syntactically valid but absent.
        result = self._run(self._command(2_147_483_647))
        metadata = self._assert_error(
            result, exit_code=73, code="untrusted_process"
        )
        self.assertEqual(metadata["phase"], "process_preflight")


if __name__ == "__main__":
    unittest.main()
