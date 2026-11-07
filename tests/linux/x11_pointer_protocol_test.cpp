// Private-Xvfb test: pointer targeting must not depend on keyboard focus.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <sstream>
#include <stdexcept>

void RequirePointer(bool value, const char *message) {
    if (!value) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        setenv("XDG_CURRENT_DESKTOP", "KDE", 1);
        Display *display = XOpenDisplay(nullptr);
        RequirePointer(display != nullptr, "private X server unavailable");
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 200, 200, 0, 0, 0);
        const Window other = XCreateSimpleWindow(display, DefaultRootWindow(display), 300, 20, 200, 200, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = static_cast<unsigned long>(getpid());
        const unsigned long wrong_pid = pid + 1;
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XChangeProperty(display, other, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&wrong_pid), 1);
        XSelectInput(display, target, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask | PointerMotionMask);
        XSelectInput(display, other, KeyPressMask | KeyReleaseMask | ButtonPressMask | ButtonReleaseMask | PointerMotionMask);
        XMapWindow(display, target);
        XMapWindow(display, other);
        XSetInputFocus(display, other, RevertToParent, CurrentTime);
        XSync(display, False);
        auto invoke = [&](std::vector<std::string> strings) {
            const auto now = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count();
            strings.insert(strings.begin(), "helper");
            strings.insert(strings.end(), {"--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(now + 300000000LL)});
            std::vector<char *> args;
            for (auto &arg : strings) args.push_back(arg.data());
            std::ostringstream output;
            auto *saved = std::cout.rdbuf(output.rdbuf());
            const int code = aad_xtest_helper_main(static_cast<int>(args.size()), args.data());
            std::cout.rdbuf(saved);
            return std::pair<int, std::string>{code, output.str()};
        };
        const auto hover = invoke({"pointer-gesture", "--mode", "hover", "--x", "30", "--y", "30", "--hover-ms", "1"});
        RequirePointer(hover.first == 0 && hover.second.find("\"ok\":true") != std::string::npos, "pointer must work when another process owns keyboard focus");
        Window focused = None;
        int revert = 0;
        XGetInputFocus(display, &focused, &revert);
        RequirePointer(focused == other, "hover stole keyboard focus");
        const auto click = invoke({"pointer-gesture", "--mode", "click", "--x", "30", "--y", "30", "--button", "1", "--count", "1"});
        RequirePointer(click.first == 0, "hit-proven click was blocked by focus");
        const auto wrong = invoke({"pointer-gesture", "--mode", "hover", "--x", "310", "--y", "30", "--hover-ms", "1"});
        RequirePointer(wrong.first == kExitTargetMismatch && wrong.second.find("\"dispatch_started\":false") != std::string::npos, "wrong-process pointer target must fail before dispatch");
        const auto key = invoke({"key-chord", "--keys", "enter"});
        RequirePointer(key.first == kExitTargetMismatch && key.second.find("\"dispatch_started\":false") != std::string::npos, "keyboard input must still require focus ownership");
        XSync(display, False);
        bool clicked = false;
        while (XPending(display)) {
            XEvent event{};
            XNextEvent(display, &event);
            RequirePointer(event.type != KeyPress && event.type != KeyRelease, "keyboard input escaped failed preflight");
            if (event.type == ButtonPress || event.type == ButtonRelease) {
                RequirePointer(event.xbutton.window == target, "click reached wrong process");
                clicked = true;
            }
        }
        RequirePointer(clicked, "click was not independently observed by target");
        XCloseDisplay(display);
        std::cout << "PASS: independent pointer focus, real click readback, target PID guard, keyboard focus guard\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n';
        return 1;
    }
}
