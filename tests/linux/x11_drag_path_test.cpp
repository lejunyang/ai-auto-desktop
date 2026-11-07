// Private X server only. Independently observe real XTest events and held
// state; the test itself plays the AX-validation peer of the private protocol.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main
#include <stdexcept>

static void RequirePath(bool value, const char *message) {
    if (!value) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        Display *display = XOpenDisplay(nullptr);
        RequirePath(display != nullptr, "private display unavailable");
        XSetErrorHandler(HandleXError);
        const unsigned long pid = getpid();
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 300, 250, 0, 0, 0);
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XSelectInput(display, target, ButtonPressMask | ButtonReleaseMask | PointerMotionMask | KeyPressMask | KeyReleaseMask);
        XMapWindow(display, target);
        XSetInputFocus(display, target, RevertToParent, CurrentTime);
        const std::string nonce = "0123456789abcdef0123456789abcdef";
        for (const std::string mode : {"success", "bad_nonce", "bad_index", "oversize", "eof", "deadline", "cancel", "geometry", "overlay", "preheld"}) {
            XMoveWindow(display, target, 20, 20);
            XRaiseWindow(display, target);
            XSetInputFocus(display, target, RevertToParent, CurrentTime);
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); }
            if (mode == "preheld") { XTestFakeButtonEvent(display, 1, True, 0); XSync(display, False); }
            const auto until_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count()
                + (mode == "deadline" ? 200000000LL : 3000000000LL);
            std::vector<std::string> args = {"helper", "drag", "--expected-pid", std::to_string(pid),
                "--window-id", std::to_string(target), "--window-x", "20", "--window-y", "20", "--window-width", "300", "--window-height", "250",
                "--x", "50", "--y", "60", "--to-x", "260", "--to-y", "210", "--button", "1",
                "--path", "250,70;70,190", "--path-token", nonce, "--modifiers", "ctrl",
                "--deadline-monotonic-ns", std::to_string(until_ns)};
            int input[2], output[2];
            RequirePath(pipe(input) == 0 && pipe(output) == 0, "pipe failed");
            const pid_t worker = fork(); RequirePath(worker >= 0, "fork failed");
            if (worker == 0) {
                close(ConnectionNumber(display));
                close(input[1]); dup2(input[0], STDIN_FILENO); close(input[0]);
                close(output[0]); dup2(output[1], STDOUT_FILENO); close(output[1]);
                std::vector<char *> raw; for (auto &arg : args) raw.push_back(arg.data());
                _exit(aad_xtest_helper_main(static_cast<int>(raw.size()), raw.data()));
            }
            close(input[0]); close(output[1]);
            fcntl(output[0], F_SETFL, fcntl(output[0], F_GETFL) | O_NONBLOCK);
            std::string transcript; std::size_t cursor = 0; int status = 0;
            unsigned downs = 0, ups = 0, key_downs = 0, key_ups = 0, validations = 0;
            std::vector<std::pair<int,int>> trace;
            Window overlay = None;
            const auto started = std::chrono::steady_clock::now();
            auto events = [&] {
                XSync(display, False);
                while (XPending(display)) {
                    XEvent event{}; XNextEvent(display, &event);
                    if (event.type == ButtonPress) ++downs;
                    if (event.type == ButtonRelease) ++ups;
                    if (event.type == KeyPress) ++key_downs;
                    if (event.type == KeyRelease) ++key_ups;
                    if (event.type == MotionNotify && (event.xmotion.state & Button1Mask)) trace.emplace_back(event.xmotion.x_root, event.xmotion.y_root);
                }
            };
            for (;;) {
                events();
                char bytes[2048]; ssize_t count;
                while ((count = read(output[0], bytes, sizeof(bytes))) > 0) transcript.append(bytes, static_cast<std::size_t>(count));
                for (;;) {
                    const auto end = transcript.find('\n', cursor);
                    if (end == std::string::npos) break;
                    const auto line = transcript.substr(cursor, end - cursor); cursor = end + 1;
                    if (line.find("validate_segment") == std::string::npos) continue;
                    const auto index = std::stoul(line.substr(line.find("\"index\":") + 8));
                    RequirePath(index == validations++, "helper requested wrong sequence");
                    std::string ack = "ack " + nonce + " " + std::to_string(index) + "\n";
                    if (index == 0 && mode == "bad_nonce") ack = "ack wrong 0\n";
                    if (index == 0 && mode == "bad_index") ack = "ack " + nonce + " 9\n";
                    if (index == 0 && mode == "oversize") ack = std::string(100, 'x') + "\n";
                    if (index == 1) {
                        if (mode == "eof") { close(input[1]); input[1] = -1; continue; }
                        if (mode == "cancel") { RequirePath(kill(worker, SIGTERM) == 0, "cancel failed"); continue; }
                        if (mode == "deadline") continue;
                        if (mode == "geometry") { XMoveWindow(display, target, 30, 20); XSync(display, False); continue; }
                        if (mode == "overlay") {
                            overlay = XCreateSimpleWindow(display, DefaultRootWindow(display), 40, 40, 60, 60, 0, 0, 0);
                            XChangeProperty(display, overlay, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
                            XMapRaised(display, overlay); XSync(display, False); continue;
                        }
                    }
                    RequirePath(write(input[1], ack.data(), ack.size()) == static_cast<ssize_t>(ack.size()), "ACK write failed");
                }
                if (waitpid(worker, &status, WNOHANG) == worker) break;
                if (std::chrono::steady_clock::now() - started > std::chrono::seconds(4)) {
                    kill(worker, SIGKILL); waitpid(worker, &status, 0); throw std::runtime_error("helper did not release within deadline");
                }
                usleep(1000);
            }
            events();
            char bytes[2048]; ssize_t count;
            while ((count = read(output[0], bytes, sizeof(bytes))) > 0) transcript.append(bytes, static_cast<std::size_t>(count));
            close(output[0]); if (input[1] >= 0) close(input[1]);
            if (overlay != None) XDestroyWindow(display, overlay);
            RequirePath(WIFEXITED(status), "helper did not exit cooperatively");
            if (mode == "success") {
                RequirePath(WEXITSTATUS(status) == 0 && validations == 5 && downs == 1 && ups == 1, "path dispatch/validation count mismatch");
                RequirePath(trace == std::vector<std::pair<int,int>>{{250,70},{70,190},{260,210}}, "actual held pointer trajectory differs from anchors");
            } else if (mode == "preheld") {
                RequirePath(validations == 0 && ups == 0 && key_downs == 0, "helper took ownership of external held input");
                XTestFakeButtonEvent(display, 1, False, 0); XSync(display, False);
            } else if (mode == "bad_nonce" || mode == "bad_index" || mode == "oversize") {
                RequirePath(downs == 0 && key_downs == 0 && trace.empty(), "invalid ACK dispatched input");
            } else {
                RequirePath(downs == 1 && ups == 1 && trace.empty(), "failed validation dispatched movement or left button held");
                RequirePath(std::chrono::steady_clock::now() - started < std::chrono::seconds(1), "failure waited for full request budget");
            }
            RequirePath(key_downs == key_ups, "owned modifier was not released");
            char keys[32]{}; XQueryKeymap(display, keys);
            for (char key : keys) RequirePath(key == 0, "modifier remains held");
            Window root = None, child = None; int rx = 0, ry = 0, wx = 0, wy = 0; unsigned mask = 0;
            RequirePath(XQueryPointer(display, DefaultRootWindow(display), &root, &child, &rx, &ry, &wx, &wy, &mask) && !(mask & Button1Mask), "button remains held");
            std::cout << "PASS " << mode << " downs=" << downs << " ups=" << ups << " moves=" << trace.size() << " validations=" << validations << '\n';
        }
        XCloseDisplay(display);
        return 0;
    } catch (const std::exception &error) { std::cerr << "FAIL: " << error.what() << '\n'; return 1; }
}
