// Run only on a private Xvfb. The desktop label must not replace X11 proofs.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <sstream>
#include <stdexcept>

void RequireSession(bool value, const char *message) {
    if (!value) throw std::runtime_error(message);
}

std::string SessionDeadline() {
    const auto now = std::chrono::duration_cast<std::chrono::nanoseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
    return std::to_string(now + 1000000000LL);
}

struct CaptureResult {
    int status;
    std::string png;
    std::string metadata;
};

CaptureResult CaptureSession(const char *binary, unsigned long pid, int x) {
    int out[2], error[2];
    RequireSession(pipe(out) == 0 && pipe(error) == 0, "capture pipe failed");
    std::vector<std::string> strings{binary, "capture-target", "--expected-pid", std::to_string(pid),
        "--x", std::to_string(x), "--y", "30", "--width", "16", "--height", "16",
        "--deadline-monotonic-ns", SessionDeadline()};
    std::vector<char *> args;
    for (auto &arg : strings) args.push_back(arg.data());
    args.push_back(nullptr);
    const pid_t child = fork();
    RequireSession(child >= 0, "capture fork failed");
    if (child == 0) {
        dup2(out[1], STDOUT_FILENO);
        dup2(error[1], STDERR_FILENO);
        close(out[0]); close(out[1]); close(error[0]); close(error[1]);
        execv(binary, args.data());
        _exit(127);
    }
    close(out[1]); close(error[1]);
    auto read_all = [](int fd) {
        std::string result;
        char buffer[4096];
        for (;;) {
            const ssize_t count = read(fd, buffer, sizeof(buffer));
            if (count <= 0) break;
            result.append(buffer, static_cast<std::size_t>(count));
        }
        close(fd);
        return result;
    };
    CaptureResult result{0, read_all(out[0]), read_all(error[0])};
    int status = 0;
    RequireSession(waitpid(child, &status, 0) == child && WIFEXITED(status), "capture child failed");
    result.status = WEXITSTATUS(status);
    return result;
}

int main(int argc, char **argv) {
    try {
        RequireSession(argc == 2, "capture helper path required");
        Display *display = XOpenDisplay(nullptr);
        RequireSession(display != nullptr, "private X server unavailable");
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 200, 200, 0, 0, 0x44aaff);
        const Window other = XCreateSimpleWindow(display, DefaultRootWindow(display), 300, 20, 200, 200, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = static_cast<unsigned long>(getpid());
        const unsigned long wrong_pid = pid + 1;
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XChangeProperty(display, other, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&wrong_pid), 1);
        XSelectInput(display, target, ButtonPressMask | ButtonReleaseMask | KeyPressMask | KeyReleaseMask);
        XMapWindow(display, target); XMapWindow(display, other);
        XSetInputFocus(display, target, RevertToParent, CurrentTime);
        XSync(display, False);
        auto invoke = [&](std::vector<std::string> strings) {
            strings.insert(strings.begin(), "helper");
            strings.insert(strings.end(), {"--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", SessionDeadline()});
            std::vector<char *> args;
            for (auto &arg : strings) args.push_back(arg.data());
            std::ostringstream output;
            auto *saved = std::cout.rdbuf(output.rdbuf());
            const int status = aad_xtest_helper_main(static_cast<int>(args.size()), args.data());
            std::cout.rdbuf(saved);
            return std::pair<int, std::string>{status, output.str()};
        };
        auto click = [&](int x) {
            return invoke({"pointer-gesture", "--mode", "click", "--x", std::to_string(x), "--y", "30", "--button", "1", "--count", "1"});
        };
        for (const char *desktop : {"KDE", "GNOME", "XFCE", ""}) {
            setenv("XDG_SESSION_TYPE", "x11", 1);
            if (*desktop) setenv("XDG_CURRENT_DESKTOP", desktop, 1);
            else { unsetenv("XDG_CURRENT_DESKTOP"); unsetenv("DESKTOP_SESSION"); }
            RequireSession(click(30).first == 0, "X11 click blocked by desktop label");
            XSync(display, False);
            bool pressed = false;
            while (XPending(display)) {
                XEvent event{}; XNextEvent(display, &event);
                if (event.type == ButtonPress) {
                    RequireSession(event.xbutton.window == target, "click reached another window");
                    pressed = true;
                }
            }
            RequireSession(pressed, "target did not observe actual click");
            const auto capture = CaptureSession(argv[1], pid, 30);
            RequireSession(capture.status == 0 && capture.png.compare(0, 8, "\x89PNG\r\n\x1a\n", 8) == 0,
                "X11 target capture blocked by desktop label");
            RequireSession(capture.metadata.find("\"same_euid_verified\":true") != std::string::npos,
                "capture lost process ownership proof");
        }
        setenv("XDG_CURRENT_DESKTOP", "GNOME", 1);
        const auto wrong_point = click(310);
        RequireSession(wrong_point.first == kExitTargetMismatch && wrong_point.second.find("\"dispatch_started\":false") != std::string::npos,
            "GNOME label bypassed pointer PID guard");
        RequireSession(CaptureSession(argv[1], pid, 310).status != 0, "GNOME label bypassed capture PID guard");
        XSetInputFocus(display, other, RevertToParent, CurrentTime); XSync(display, False);
        const auto key = invoke({"key-chord", "--keys", "enter"});
        RequireSession(key.first == kExitTargetMismatch, "GNOME label bypassed keyboard focus guard");
        for (const char *session : {"wayland", "", "unspecified"}) {
            setenv("XDG_SESSION_TYPE", session, 1);
            const auto refused = click(30);
            RequireSession(refused.first == kExitUnavailable && refused.second.find("\"dispatch_started\":false") != std::string::npos,
                "non-X11 session dispatched pointer input");
            const auto capture = CaptureSession(argv[1], pid, 30);
            RequireSession(capture.status != 0 && capture.png.empty() && capture.metadata.find("unsupported_session") != std::string::npos,
                "non-X11 session captured pixels");
        }
        XSync(display, False);
        RequireSession(XPending(display) == 0, "rejected operations produced input events");
        XCloseDisplay(display);
        std::cout << "PASS: X11 desktop labels, actual click/capture, PID/focus guards, Wayland rejection\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n';
        return 1;
    }
}
