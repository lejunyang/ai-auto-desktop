// Private-Xvfb regression for modifier-bearing pointer gestures and cleanup.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <sstream>
#include <stdexcept>

void RequireModifier(bool value, const char *message) {
    if (!value) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        setenv("XDG_CURRENT_DESKTOP", "KDE", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireModifier(display != nullptr, "private X server unavailable");
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 200, 200, 0, 0, 0);
        const Window other = XCreateSimpleWindow(display, DefaultRootWindow(display), 300, 20, 200, 200, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = static_cast<unsigned long>(getpid());
        const unsigned long wrong_pid = pid + 1;
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XChangeProperty(display, other, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&wrong_pid), 1);
        XSelectInput(display, target, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask | PointerMotionMask);
        XMapWindow(display, target);
        XMapWindow(display, other);
        XSetInputFocus(display, other, RevertToParent, CurrentTime);
        XSync(display, False);
        auto call = [&](std::vector<std::string> strings, long long duration_ns) {
            const auto now = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count();
            strings.insert(strings.begin(), "helper");
            strings.insert(strings.end(), {"--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(now + duration_ns)});
            std::vector<char *> args;
            for (auto &arg : strings) args.push_back(arg.data());
            return aad_xtest_helper_main(static_cast<int>(args.size()), args.data());
        };
        auto invoke = [&](std::vector<std::string> strings, long long duration_ns = 300000000LL) {
            std::ostringstream output;
            auto *saved = std::cout.rdbuf(output.rdbuf());
            const int code = call(std::move(strings), duration_ns);
            std::cout.rdbuf(saved);
            return std::pair<int, std::string>{code, output.str()};
        };
        std::vector<std::string> click = {"pointer-gesture", "--mode", "click", "--x", "30", "--y", "30", "--button", "1", "--count", "1", "--modifiers", "ctrl"};
        const auto unfocused = invoke(click);
        RequireModifier(unfocused.first == kExitTargetMismatch && unfocused.second.find("\"dispatch_started\":false") != std::string::npos,
                        "modifier click must not send keys to another process");
        XSetInputFocus(display, target, RevertToParent, CurrentTime);
        XSync(display, False);
        RequireModifier(invoke(click).first == 0, "Ctrl-click failed");
        XSync(display, False);
        bool ctrl_click = false;
        while (XPending(display)) {
            XEvent event{};
            XNextEvent(display, &event);
            if (event.type == ButtonPress) ctrl_click |= (event.xbutton.state & ControlMask) != 0;
        }
        RequireModifier(ctrl_click, "target did not observe Control on ButtonPress");
        auto released = [&]() {
            char keys[32]{};
            XQueryKeymap(display, keys);
            const KeyCode ctrl = XKeysymToKeycode(display, XK_Control_L);
            Window root = None, child = None;
            int rx = 0, ry = 0, wx = 0, wy = 0;
            unsigned mask = 0;
            XQueryPointer(display, target, &root, &child, &rx, &ry, &wx, &wy, &mask);
            return (keys[ctrl / 8] & (1 << (ctrl % 8))) == 0 && (mask & Button1Mask) == 0;
        };
        RequireModifier(released(), "successful click left input held");
        click.back() = "ctrl,ctrl";
        RequireModifier(invoke(click).first == kExitInvalid, "duplicate modifiers accepted");
        std::vector<std::string> drag = {"drag", "--x", "30", "--y", "30", "--to-x", "180", "--to-y", "180", "--button", "1", "--modifiers", "ctrl"};
        RequireModifier(invoke(drag, 70000000LL).first == kExitUnknownEffect, "deadline did not interrupt the drag");
        RequireModifier(released(), "deadline left a modifier or button held");
        int output[2];
        RequireModifier(pipe(output) == 0, "cannot create child output pipe");
        const pid_t worker = fork();
        RequireModifier(worker >= 0, "cannot fork helper");
        if (worker == 0) {
            close(output[0]);
            dup2(output[1], STDOUT_FILENO);
            close(output[1]);
            _exit(call(drag, 3000000000LL));
        }
        close(output[1]);
        pollfd ready{output[0], POLLIN, 0};
        RequireModifier(poll(&ready, 1, 1000) > 0, "helper did not start dispatch");
        char started[256]{};
        RequireModifier(read(output[0], started, sizeof(started)) > 0, "missing dispatch event");
        usleep(30000);
        kill(worker, SIGTERM);
        int status = 0;
        waitpid(worker, &status, 0);
        close(output[0]);
        RequireModifier(WIFEXITED(status) && WEXITSTATUS(status) == kExitUnknownEffect, "termination bypassed cleanup");
        RequireModifier(released(), "termination left a modifier or button held");
        XCloseDisplay(display);
        std::cout << "PASS: Ctrl-click state, focus guard, duplicates, deadline and termination cleanup\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n';
        return 1;
    }
}
