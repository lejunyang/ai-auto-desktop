// Actual event durations and cleanup on a private Xvfb, never the user desktop.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <functional>
#include <stdexcept>

void RequireHold(bool condition, const char *message) {
    if (!condition) throw std::runtime_error(message);
}

struct HoldResult {
    int code = -1;
    std::string output;
    std::vector<XEvent> events;
};

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireHold(display != nullptr, "private X server unavailable");
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 200, 200, 0, 0, 0);
        const Window other = XCreateSimpleWindow(display, DefaultRootWindow(display), 300, 20, 200, 200, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = static_cast<unsigned long>(getpid());
        const unsigned long wrong_pid = pid + 1;
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XChangeProperty(display, other, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&wrong_pid), 1);
        XSelectInput(display, target, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask | PointerMotionMask);
        XMapWindow(display, target); XMapWindow(display, other);
        XSetInputFocus(display, target, RevertToParent, CurrentTime); XSync(display, False);
        const KeyCode a = XKeysymToKeycode(display, XK_a);
        const KeyCode ctrl = XKeysymToKeycode(display, XK_Control_L);
        auto key_down = [&](KeyCode code) {
            char keys[32]{}; XQueryKeymap(display, keys);
            return (keys[code / 8] & (1 << (code % 8))) != 0;
        };
        auto button_down = [&]() {
            Window root = None, child = None; int rx = 0, ry = 0, wx = 0, wy = 0; unsigned mask = 0;
            RequireHold(XQueryPointer(display, target, &root, &child, &rx, &ry, &wx, &wy, &mask), "cannot query pointer");
            return (mask & Button1Mask) != 0;
        };
        auto released = [&]() { return !key_down(a) && !key_down(ctrl) && !button_down(); };
        auto invoke = [&](std::vector<std::string> args, unsigned deadline_ms = 2000,
                          std::function<void(const XEvent &, pid_t)> on_event = {}) {
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); }
            int output[2]; RequireHold(pipe(output) == 0, "cannot create output pipe");
            const auto now = std::chrono::steady_clock::now();
            const auto deadline = std::chrono::duration_cast<std::chrono::nanoseconds>(now.time_since_epoch()).count() + deadline_ms * 1000000LL;
            args.insert(args.begin(), "helper");
            args.insert(args.end(), {"--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(deadline)});
            const pid_t worker = fork(); RequireHold(worker >= 0, "cannot fork helper");
            if (worker == 0) {
                close(output[0]); dup2(output[1], STDOUT_FILENO); close(output[1]);
                std::vector<char *> argv;
                for (auto &arg : args) argv.push_back(arg.data());
                _exit(aad_xtest_helper_main(static_cast<int>(argv.size()), argv.data()));
            }
            close(output[1]);
            HoldResult result; int status = 0;
            for (;;) {
                XSync(display, False);
                while (XPending(display)) {
                    XEvent event{}; XNextEvent(display, &event); result.events.push_back(event);
                    if (on_event) on_event(event, worker);
                }
                if (waitpid(worker, &status, WNOHANG) == worker) break;
                if (std::chrono::steady_clock::now() - now > std::chrono::seconds(6)) {
                    kill(worker, SIGTERM); waitpid(worker, &status, 0);
                    throw std::runtime_error("helper exceeded bounded execution");
                }
                usleep(1000);
            }
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); result.events.push_back(event); }
            char buffer[4096]; ssize_t count;
            while ((count = read(output[0], buffer, sizeof(buffer))) > 0) result.output.append(buffer, static_cast<std::size_t>(count));
            close(output[0]);
            RequireHold(WIFEXITED(status), "helper terminated without cleanup");
            result.code = WEXITSTATUS(status);
            return result;
        };
        std::vector<std::string> key{"key-chord", "--keys", "a", "--hold-ms", "200"};
        std::vector<std::string> click{"pointer-gesture", "--mode", "click", "--x", "30", "--y", "30", "--button", "1", "--count", "1", "--hold-ms", "200"};
        std::vector<std::string> drag{"drag", "--x", "30", "--y", "30", "--to-x", "180", "--to-y", "180", "--button", "1", "--hold-ms", "200"};
        auto duration = [&](const HoldResult &result, bool keyboard) {
            Time down = 0, up = 0;
            for (const auto &event : result.events) {
                if (keyboard && event.type == KeyPress && event.xkey.keycode == a) down = event.xkey.time;
                if (keyboard && event.type == KeyRelease && event.xkey.keycode == a) up = event.xkey.time;
                if (!keyboard && event.type == ButtonPress) down = event.xbutton.time;
                if (!keyboard && event.type == ButtonRelease) up = event.xbutton.time;
            }
            RequireHold(down != 0 && up >= down, "missing actual press/release events");
            return up - down;
        };
        const auto held_key = invoke(key);
        RequireHold(held_key.code == 0 && duration(held_key, true) >= 190 && released(), "key was not held for requested duration");
        const auto held_click = invoke(click);
        RequireHold(held_click.code == 0 && duration(held_click, false) >= 190 && released(), "button was not held for requested duration");
        const auto held_drag = invoke(drag);
        Time drag_down = 0, first_move = 0;
        for (const auto &event : held_drag.events) {
            if (event.type == ButtonPress) drag_down = event.xbutton.time;
            if (event.type == MotionNotify && (event.xmotion.state & Button1Mask) != 0 && !first_move) first_move = event.xmotion.time;
        }
        RequireHold(held_drag.code == 0 && drag_down != 0 && first_move - drag_down >= 190 && released(), "drag moved before its bounded hold");
        for (auto args : {key, click, drag}) {
            args.back() = "500";
            const auto timed = invoke(args, 80);
            RequireHold(timed.code == kExitUnknownEffect && timed.output.find("deadline_exceeded") != std::string::npos && released(), "deadline left input held");
        }
        bool changed = false;
        auto interrupted_click = click; interrupted_click.back() = "500";
        interrupted_click.insert(interrupted_click.end(), {"--modifiers", "ctrl"});
        const auto stopped = invoke(interrupted_click, 2000, [&](const XEvent &event, pid_t worker) {
            if (event.type == ButtonPress && !changed) { changed = true; kill(worker, SIGTERM); }
        });
        RequireHold(changed && stopped.code == kExitUnknownEffect && released(), "termination left button or modifier held");
        changed = false;
        auto long_key = key; long_key.back() = "500";
        const auto lost_focus = invoke(long_key, 2000, [&](const XEvent &event, pid_t) {
            if (event.type == KeyPress && !changed) { changed = true; XSetInputFocus(display, other, RevertToParent, CurrentTime); XSync(display, False); }
        });
        RequireHold(changed && lost_focus.code == kExitUnknownEffect && lost_focus.output.find("focus_owner_changed") != std::string::npos && released(), "focus loss left key held");
        XSetInputFocus(display, target, RevertToParent, CurrentTime); XSync(display, False);
        changed = false;
        const auto covered = invoke(interrupted_click, 2000, [&](const XEvent &event, pid_t) {
            if (event.type == ButtonPress && !changed) { changed = true; XMoveWindow(display, other, 20, 20); XRaiseWindow(display, other); XSync(display, False); }
        });
        RequireHold(changed && covered.code == kExitUnknownEffect && released(), "occlusion left button or modifier held");
        XMoveWindow(display, other, 300, 20); XSync(display, False);
        XTestFakeKeyEvent(display, a, True, 0); XSync(display, False);
        const auto external_key = invoke(key);
        RequireHold(external_key.code == kExitTargetMismatch && key_down(a) && external_key.events.empty(), "preheld key was taken over or released");
        XTestFakeKeyEvent(display, a, False, 0); XSync(display, False);
        XTestFakeButtonEvent(display, 1, True, 0); XSync(display, False);
        const auto external_button = invoke(click);
        RequireHold(external_button.code == kExitTargetMismatch && button_down() && external_button.events.empty(), "preheld button was taken over or released");
        XTestFakeButtonEvent(display, 1, False, 0); XSync(display, False);
        for (const auto &invalid : {"0", "5001", "-1", "1.5"}) {
            auto args = click; args.back() = invalid;
            const auto bad = invoke(args);
            RequireHold(bad.code == kExitInvalid && bad.events.empty(), "invalid hold dispatched input");
        }
        auto double_click = click; double_click[10] = "2";
        const auto invalid_double = invoke(double_click);
        RequireHold(invalid_double.code == kExitInvalid && invalid_double.events.empty(), "double click accepted bounded hold");
        RequireHold(released(), "test left input held");
        XCloseDisplay(display);
        std::cout << "PASS: observed key/button/drag durations, deadline/focus/occlusion/termination cleanup, external-held input preserved\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n';
        return 1;
    }
}
