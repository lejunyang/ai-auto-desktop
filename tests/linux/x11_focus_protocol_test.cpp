// Run only on the private Xvfb supplied by verify-x11-focus.ps1.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <sstream>
#include <stdexcept>

void RequireFocus(bool value, const char *message) {
    if (!value) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        setenv("XDG_CURRENT_DESKTOP", "KDE", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireFocus(display != nullptr, "private X server unavailable");
        const Window window = XCreateSimpleWindow(display, DefaultRootWindow(display), 0, 0, 100, 100, 0, 0, 0);
        const Window child = XCreateSimpleWindow(display, window, 0, 0, 20, 20, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = static_cast<unsigned long>(getpid());
        XChangeProperty(display, window, pid_atom, XA_CARDINAL, 32, PropModeReplace,
                        reinterpret_cast<const unsigned char *>(&pid), 1);
        XSelectInput(display, child, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask | PointerMotionMask);
        XMapWindow(display, window);
        XMapWindow(display, child);
        XSetInputFocus(display, child, RevertToParent, CurrentTime);
        XSync(display, False);
        auto invoke = [&](std::uint64_t deadline) {
            std::vector<std::string> strings = {"helper", "focus-info", "--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(deadline)};
            std::vector<char *> args;
            for (auto &arg : strings) args.push_back(arg.data());
            std::ostringstream output;
            auto *saved = std::cout.rdbuf(output.rdbuf());
            const int code = RunFocusInfo(static_cast<int>(args.size()), args.data());
            std::cout.rdbuf(saved);
            return std::pair<int, std::string>{code, output.str()};
        };
        const auto until = static_cast<std::uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count()) + 10000000000ULL;
        const auto focused = invoke(until);
        RequireFocus(focused.first == 0 && focused.second.find("\"focused\":true") != std::string::npos,
                     "child focus must resolve parent process ownership");
        RequireFocus(focused.second.find("dispatch") == std::string::npos, "read-only query reported dispatch");
        const unsigned long other = pid + 1;
        XChangeProperty(display, child, pid_atom, XA_CARDINAL, 32, PropModeReplace,
                        reinterpret_cast<const unsigned char *>(&other), 1);
        XSync(display, False);
        const auto mismatch = invoke(until);
        RequireFocus(mismatch.first == 0 && mismatch.second.find("\"focused\":false") != std::string::npos,
                     "explicit focused-window PID mismatch must fail closed");
        RequireFocus(invoke(0).first == kExitTimeoutBeforeDispatch, "expired read must fail");
        Window actual = None;
        int revert = 0;
        XGetInputFocus(display, &actual, &revert);
        XSync(display, False);
        RequireFocus(actual == child && XPending(display) == 0, "query changed focus or emitted input");
        XCloseDisplay(display);
        std::cout << "PASS: focus ancestry, PID mismatch, expired deadline, no input or focus changes\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n';
        return 1;
    }
}
