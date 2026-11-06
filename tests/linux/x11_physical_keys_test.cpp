// Explicit physical typing uses only a private X server's existing keymap.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main
#include <stdexcept>

static void RequireKeys(bool ok, const char *message) {
    if (!ok) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireKeys(display != nullptr, "private display unavailable");
        XSetErrorHandler(HandleXError);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = getpid();
        auto window = [&](int x) {
            const Window result = XCreateSimpleWindow(display, DefaultRootWindow(display), x, 20, 180, 180, 0, 0, 0);
            XChangeProperty(display, result, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
            XSelectInput(display, result, KeyPressMask | KeyReleaseMask);
            XMapWindow(display, result);
            return result;
        };
        const Window target = window(20), other = window(240);
        XSetInputFocus(display, target, RevertToParent, CurrentTime);
        XSync(display, False);
        int minimum = 0, maximum = 0;
        XDisplayKeycodes(display, &minimum, &maximum);
        auto mapping = [&]() {
            int width = 0;
            KeySym *symbols = XGetKeyboardMapping(display, static_cast<KeyCode>(minimum), maximum - minimum + 1, &width);
            RequireKeys(symbols != nullptr && width > 0, "cannot read keymap");
            std::vector<KeySym> result(symbols, symbols + (maximum - minimum + 1) * width);
            XFree(symbols);
            return result;
        };
        const auto original_mapping = mapping();
        unsigned test_index = 0;
        struct Result { unsigned presses = 0, releases = 0; int exit_code = -1; std::string text, output; };
        auto invoke = [&](const std::string &text, const std::string &interruption = "", bool wrong_window = false, bool binding = true) {
            ++test_index;
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); }
            const auto deadline = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() + (wrong_window ? 150000000LL : 4000000000LL);
            std::vector<std::string> args{"helper", "type-text", "--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(deadline), "--mode", "keys"};
            if (binding) args.insert(args.end(), {"--window-id", std::to_string(target), "--window-x", "20", "--window-y", "20", "--window-width", "180", "--window-height", "180"});
            int input[2], output[2];
            RequireKeys(pipe(input) == 0 && pipe(output) == 0, "pipe failed");
            const pid_t worker = fork(); RequireKeys(worker >= 0, "fork failed");
            if (worker == 0) {
                close(ConnectionNumber(display));
                close(input[1]); dup2(input[0], STDIN_FILENO); close(input[0]);
                close(output[0]); dup2(output[1], STDOUT_FILENO); close(output[1]);
                std::vector<char *> raw; for (auto &arg : args) raw.push_back(arg.data());
                _exit(aad_xtest_helper_main(static_cast<int>(raw.size()), raw.data()));
            }
            close(input[0]); close(output[1]);
            RequireKeys(write(input[1], text.data(), text.size()) == static_cast<ssize_t>(text.size()), "stdin write failed");
            close(input[1]);
            Result result;
            bool interrupted = false;
            auto expected_mapping = original_mapping;
            auto drain = [&]() {
                XSync(display, False);
                while (XPending(display)) {
                    XEvent event{}; XNextEvent(display, &event);
                    if (event.type == KeyRelease) ++result.releases;
                    if (event.type != KeyPress) continue;
                    ++result.presses;
                    unsigned consumed = 0;
                    KeySym symbol = NoSymbol;
                    XkbLookupKeySym(display, static_cast<KeyCode>(event.xkey.keycode), event.xkey.state, &consumed, &symbol);
                    if (symbol >= 0x20 && symbol < 0x7f) result.text.push_back(static_cast<char>(symbol));
                    if (interrupted || interruption.empty()) continue;
                    interrupted = true;
                    if (interruption == "cancel") RequireKeys(kill(worker, SIGTERM) == 0, "cancel failed");
                    if (interruption == "focus") XSetInputFocus(display, other, RevertToParent, CurrentTime);
                    if (interruption == "state") XkbLockModifiers(display, XkbUseCoreKbd, LockMask, LockMask);
                    if (interruption == "mapping") {
                        const KeyCode a = XKeysymToKeycode(display, XK_a);
                        int width = 0; KeySym *symbols = XGetKeyboardMapping(display, a, 1, &width);
                        RequireKeys(symbols != nullptr && width > 0, "cannot change fixture keymap");
                        symbols[0] = XK_b;
                        XChangeKeyboardMapping(display, a, width, symbols, 1);
                        XFree(symbols);
                    }
                    XSync(display, False);
                    if (interruption == "mapping") expected_mapping = mapping();
                }
            };
            int status = 0;
            const auto until = std::chrono::steady_clock::now() + std::chrono::seconds(5);
            for (;;) {
                drain();
                if (waitpid(worker, &status, WNOHANG) == worker) break;
                if (std::chrono::steady_clock::now() > until) {
                    kill(worker, SIGKILL); waitpid(worker, &status, 0);
                    throw std::runtime_error("physical helper did not terminate");
                }
                usleep(200);
            }
            drain();
            char bytes[4096]; ssize_t count = 0;
            while ((count = read(output[0], bytes, sizeof(bytes))) > 0) result.output.append(bytes, static_cast<std::size_t>(count));
            close(output[0]);
            RequireKeys(WIFEXITED(status), "helper did not exit cleanly");
            result.exit_code = WEXITSTATUS(status);
            if (!interruption.empty()) {
                RequireKeys(interrupted && result.presses > 0 && result.presses < text.size(), "interruption did not stop remaining strokes");
                RequireKeys(result.presses == result.releases, "owned key prefix was not released");
                RequireKeys(result.exit_code != 0 && result.output.find("\"dispatch_started\":true") != std::string::npos, "interrupted input did not report uncertainty");
            }
            if (interruption == "state") XkbLockModifiers(display, XkbUseCoreKbd, LockMask, 0);
            XSetInputFocus(display, target, RevertToParent, CurrentTime);
            XSync(display, False);
            const auto after = mapping();
            if (after != expected_mapping) {
                std::cerr << "map mismatch in case " << test_index << ": before " << expected_mapping.size() << " after " << after.size() << '\n';
            }
            RequireKeys(after == expected_mapping, "physical mode modified the keyboard map");
            return result;
        };

        const auto ordinary = invoke("nihao Ab!");
        RequireKeys(ordinary.exit_code == 0 && ordinary.text == "nihao Ab!" && ordinary.presses == ordinary.releases, "existing keyboard map did not produce requested physical keys");
        const auto unsupported = invoke("a\xe4\xb8\xad");
        RequireKeys(unsupported.exit_code != 0 && unsupported.presses == 0 && unsupported.output.find("unsupported_keyboard_mapping") != std::string::npos, "unmapped text was partially dispatched");
        const auto unanchored = invoke("a", "", false, false);
        RequireKeys(unanchored.exit_code != 0 && unanchored.presses == 0, "physical mode accepted a missing window anchor");
        XSetInputFocus(display, other, RevertToParent, CurrentTime); XSync(display, False);
        const auto wrong = invoke("a", "", true);
        RequireKeys(wrong.exit_code != 0 && wrong.presses == 0, "same-PID wrong window received input");
        const KeyCode a = XKeysymToKeycode(display, XK_a);
        XTestFakeKeyEvent(display, a, True, 1); XSync(display, False);
        const auto held = invoke("a");
        char down[32]{}; XQueryKeymap(display, down);
        RequireKeys(held.exit_code != 0 && held.presses == 0 && (down[a / 8] & (1 << (a % 8))) != 0, "external held key was stolen");
        XTestFakeKeyEvent(display, a, False, 1); XSync(display, False);
        XkbLockModifiers(display, XkbUseCoreKbd, LockMask, LockMask); XSync(display, False);
        const auto caps = invoke("aA");
        RequireKeys(caps.exit_code == 0 && caps.text == "aA", "locked Caps state was not respected");
        XkbLockModifiers(display, XkbUseCoreKbd, LockMask, 0); XSync(display, False);
        invoke(std::string(1024, 'A'), "cancel");
        invoke(std::string(1024, 'a'), "focus");
        invoke(std::string(1024, 'a'), "state");
        invoke(std::string(1024, 'a'), "mapping");
        XQueryKeymap(display, down);
        for (const auto byte : down) RequireKeys(byte == 0, "owned key remained held");
        XCloseDisplay(display);
        std::cout << "PASS: existing physical map, unmapped preflight, exact window, Caps, external holds, cancel/focus/state/map changes and owned release\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n'; return 1;
    }
}
