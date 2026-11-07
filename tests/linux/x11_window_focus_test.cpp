// Same-process sibling windows must not satisfy an observed keyboard anchor.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <stdexcept>

static void RequireWindow(bool ok, const char *message) {
    if (!ok) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireWindow(display != nullptr, "private X display unavailable");
        const Window root = DefaultRootWindow(display);
        const Window target = XCreateSimpleWindow(display, root, 20, 20, 200, 200, 0, 0, 0);
        const Window sibling = XCreateSimpleWindow(display, root, 300, 20, 200, 200, 0, 0, 0);
        const Window child = XCreateSimpleWindow(display, target, 5, 5, 50, 50, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = getpid();
        for (Window window : {target, sibling}) {
            XChangeProperty(display, window, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        }
        for (Window window : {target, sibling, child}) {
            XSelectInput(display, window, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask);
            XMapWindow(display, window);
        }
        XSetInputFocus(display, sibling, RevertToParent, CurrentTime); XSync(display, False);
        const WindowBinding binding{target, {20, 20, 200, 200}};
        RequireWindow(FocusBelongsToPid(display, pid, pid_atom), "PID-only baseline not reproduced");
        RequireWindow(!FocusMatchesWindow(display, pid, pid_atom, binding), "same-PID sibling accepted");
        const KeyCode f2 = XKeysymToKeycode(display, XK_F2);
        const KeyCode ctrl = XKeysymToKeycode(display, XK_Control_L);
        auto released = [&] {
            char keys[32]{}; XQueryKeymap(display, keys);
            return (keys[f2 / 8] & (1 << (f2 % 8))) == 0 && (keys[ctrl / 8] & (1 << (ctrl % 8))) == 0;
        };
        struct Result { int code; std::string output; unsigned presses = 0; unsigned releases = 0; };
        auto invoke = [&](std::vector<std::string> args, bool geometry_only = false,
                          std::function<void(const XEvent &)> on_event = {}) {
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); }
            const auto deadline = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() + 500000000LL;
            args.insert(args.begin(), "helper");
            args.insert(args.end(), {"--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(deadline), "--window-x", "20", "--window-y", "20", "--window-width", "200", "--window-height", "200"});
            if (!geometry_only) args.insert(args.end(), {"--window-id", std::to_string(target)});
            int output[2], input[2];
            RequireWindow(pipe(output) == 0 && pipe(input) == 0, "pipe failed");
            const pid_t worker = fork(); RequireWindow(worker >= 0, "fork failed");
            if (worker == 0) {
                close(output[0]); dup2(output[1], STDOUT_FILENO); close(output[1]);
                close(input[1]); dup2(input[0], STDIN_FILENO); close(input[0]);
                std::vector<char *> raw; for (auto &arg : args) raw.push_back(arg.data());
                _exit(aad_xtest_helper_main(static_cast<int>(raw.size()), raw.data()));
            }
            close(output[1]); close(input[0]);
            RequireWindow(write(input[1], "a", 1) == 1, "stdin write failed"); close(input[1]);
            Result result{}; int status = 0;
            const auto until = std::chrono::steady_clock::now() + std::chrono::seconds(3);
            auto drain = [&] {
                XSync(display, False);
                while (XPending(display)) {
                    XEvent event{}; XNextEvent(display, &event);
                    result.presses += event.type == KeyPress || event.type == ButtonPress;
                    result.releases += event.type == KeyRelease || event.type == ButtonRelease;
                    if (on_event) on_event(event);
                }
            };
            for (;;) {
                drain();
                if (waitpid(worker, &status, WNOHANG) == worker) break;
                if (std::chrono::steady_clock::now() > until) { kill(worker, SIGTERM); waitpid(worker, &status, 0); throw std::runtime_error("window guard exceeded deadline"); }
                usleep(1000);
            }
            drain(); char bytes[4096]; ssize_t count;
            while ((count = read(output[0], bytes, sizeof(bytes))) > 0) result.output.append(bytes, static_cast<std::size_t>(count));
            close(output[0]); RequireWindow(WIFEXITED(status), "helper terminated without cleanup");
            result.code = WEXITSTATUS(status); return result;
        };
        const auto wrong_proof = invoke({"focus-info"}, true);
        RequireWindow(wrong_proof.code == 0 && wrong_proof.output.find("\"focused\":false") != std::string::npos && wrong_proof.output.find("\"window_bound\":true") != std::string::npos, "read-only proof accepted sibling");
        for (const auto &args : std::vector<std::vector<std::string>>{
                 {"key-chord", "--keys", "f2"}, {"type-text"}, {"paste"},
                 {"pointer-gesture", "--mode", "click", "--x", "80", "--y", "80", "--button", "1", "--count", "1", "--modifiers", "ctrl"}}) {
            const auto denied = invoke(args);
            RequireWindow(denied.code == kExitTargetMismatch && denied.presses == 0 && denied.releases == 0 && released(), "same-PID sibling received input");
        }
        XSetInputFocus(display, child, RevertToParent, CurrentTime); XSync(display, False);
        RequireWindow(FocusMatchesWindow(display, pid, pid_atom, binding), "child focus within observed window rejected");
        const auto proof = invoke({"focus-info"}, true);
        RequireWindow(proof.code == 0 && proof.output.find("\"focused\":true") != std::string::npos, "exact-window proof failed");
        const auto success = invoke({"key-chord", "--keys", "f2"});
        RequireWindow(success.code == 0 && success.presses == 1 && success.releases == 1 && released(), "bound key did not reach target child");
        bool changed = false;
        const auto changed_focus = invoke({"key-chord", "--keys", "ctrl,f2", "--hold-ms", "200"}, false, [&](const XEvent &event) {
            if (event.type == KeyPress && event.xkey.keycode == f2 && !changed) {
                changed = true; XSetInputFocus(display, sibling, RevertToParent, CurrentTime); XSync(display, False);
            }
        });
        RequireWindow(changed && changed_focus.code == kExitUnknownEffect && released(), "same-PID focus change left owned keys held");
        XSetInputFocus(display, child, RevertToParent, CurrentTime); XSync(display, False);
        changed = false;
        const auto moved = invoke({"key-chord", "--keys", "f2", "--hold-ms", "200"}, false, [&](const XEvent &event) {
            if (event.type == KeyPress && !changed) { changed = true; XMoveWindow(display, target, 30, 20); XSync(display, False); }
        });
        RequireWindow(changed && moved.code == kExitUnknownEffect && released(), "window movement left owned key held");
        XUnmapWindow(display, sibling); XSync(display, False);
        const auto moved_proof = invoke({"focus-info"}, true);
        RequireWindow(moved_proof.code == 0 && moved_proof.output.find("\"focused\":false") != std::string::npos, "single-window geometry mismatch accepted");
        XCloseDisplay(display);
        std::cout << "PASS: PID-only gap reproduced; strict sibling denial, child acceptance, focus/move cleanup, and exact geometry verified\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n'; return 1;
    }
}
