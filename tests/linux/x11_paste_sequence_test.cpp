// A paste consumer may read several offered formats asynchronously. Verify all
// conversions belong to one temporary offer before the original is restored.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main
#include <stdexcept>

static void RequireSequence(bool ok, const char *message) {
    if (!ok) throw std::runtime_error(message);
}

int main(int argc, char **argv) {
    try {
        const bool takeover = argc == 2 && std::string_view(argv[1]) == "--takeover";
        const bool flood = argc == 2 && std::string_view(argv[1]) == "--flood";
        RequireSequence(argc == 1 || takeover || flood, "invalid test mode");
        setenv("XDG_SESSION_TYPE", "x11", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireSequence(display != nullptr, "private display unavailable");
        XSetErrorHandler(HandleXError);
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 200, 200, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = getpid();
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XSelectInput(display, target, KeyPressMask | KeyReleaseMask);
        XMapWindow(display, target);
        XSetInputFocus(display, target, RevertToParent, CurrentTime);
        const Selection original = OpenSelection(display);
        const Selection requestor = OpenSelection(display);
        const std::string saved = "private-original";
        const std::string payload = "private-paste-\xe4\xb8\xad\xe6\x96\x87";
        const std::vector<Item> saved_items{
            {original.utf8, original.utf8, 8, saved},
            {original.plain_utf8, original.plain_utf8, 8, saved},
            {original.text, original.utf8, 8, saved},
        };
        XSetSelectionOwner(display, original.clipboard, original.window, CurrentTime);
        XSync(display, False);
        const auto deadline = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() + 5000000000LL;
        std::vector<std::string> args{"helper", "paste", "--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(deadline), "--window-id", std::to_string(target), "--window-x", "20", "--window-y", "20", "--window-width", "200", "--window-height", "200"};
        int input[2], output[2];
        RequireSequence(pipe(input) == 0 && pipe(output) == 0, "pipe failed");
        const pid_t worker = fork(); RequireSequence(worker >= 0, "fork failed");
        if (worker == 0) {
            close(ConnectionNumber(display));
            close(input[1]); dup2(input[0], STDIN_FILENO); close(input[0]);
            close(output[0]); dup2(output[1], STDOUT_FILENO); close(output[1]);
            std::vector<char *> raw; for (auto &arg : args) raw.push_back(arg.data());
            _exit(aad_xtest_helper_main(static_cast<int>(raw.size()), raw.data()));
        }
        close(input[0]); close(output[1]);
        RequireSequence(write(input[1], payload.data(), payload.size()) == static_cast<ssize_t>(payload.size()), "stdin write failed");
        close(input[1]);
        const std::vector<Atom> formats{requestor.utf8, XInternAtom(display, "text/html", False), requestor.plain_utf8, requestor.text};
        bool started = false, pending = false, exited = false;
        bool taken_over = false;
        unsigned flood_pending = 0;
        int status = 0;
        unsigned received = 0, correct = 0, unsupported = 0;
        auto next_request = std::chrono::steady_clock::now();
        auto dispatched_at = next_request;
        const auto until = next_request + std::chrono::seconds(5);
        const KeyCode v = XKeysymToKeycode(display, XK_v);
        while (std::chrono::steady_clock::now() < until) {
            XSync(display, False);
            while (XPending(display)) {
                XEvent event{}; XNextEvent(display, &event);
                if (event.type == SelectionRequest) Serve(original, event.xselectionrequest, saved_items);
                if (event.type == KeyRelease && event.xkey.keycode == v) {
                    started = true; dispatched_at = std::chrono::steady_clock::now();
                }
                if (flood && event.type == SelectionNotify && event.xselection.requestor == requestor.window && event.xselection.target == requestor.targets) {
                    if (flood_pending > 0) --flood_pending;
                    continue;
                }
                if (event.type == SelectionNotify && event.xselection.requestor == requestor.window && pending) {
                    if (event.xselection.property == None) {
                        unsupported += received == 1;
                    } else {
                        Atom type = None; int format = 0; unsigned long count = 0, remaining = 0; unsigned char *bytes = nullptr;
                        RequireSequence(XGetWindowProperty(display, requestor.window, requestor.property, 0, 4096, True, AnyPropertyType, &type, &format, &count, &remaining, &bytes) == Success, "conversion read failed");
                        if (format == 8 && remaining == 0 && std::string(bytes ? reinterpret_cast<char *>(bytes) : "", count) == payload) ++correct;
                        if (bytes) XFree(bytes);
                    }
                    ++received; pending = false;
                    if (takeover && received == 1) {
                        XSetSelectionOwner(display, original.clipboard, original.window, CurrentTime);
                        XSync(display, False); taken_over = true;
                    }
                    // Deliberately beyond the previous helper's 50 ms sleep.
                    next_request = std::chrono::steady_clock::now() + std::chrono::milliseconds(90);
                }
            }
            if (flood && received > 0 && !exited) {
                // Bound the fixture's outstanding queue while keeping requests
                // available during every helper event batch.
                while (flood_pending < 512) {
                    XConvertSelection(display, requestor.clipboard, requestor.targets, requestor.property, requestor.window, CurrentTime);
                    ++flood_pending;
                }
                XFlush(display);
            }
            if (started && !pending && !taken_over && (!flood || received == 0) && received < formats.size() && std::chrono::steady_clock::now() >= next_request) {
                XConvertSelection(display, requestor.clipboard, formats[received], requestor.property, requestor.window, CurrentTime);
                XFlush(display); pending = true;
            }
            if (!exited) exited = waitpid(worker, &status, WNOHANG) == worker;
            if (exited && (received == formats.size() || taken_over || flood)) break;
            usleep(1000);
        }
        if (!exited) { kill(worker, SIGTERM); waitpid(worker, &status, 0); }
        char buffer[4096]; std::string output_text; ssize_t count;
        while ((count = read(output[0], buffer, sizeof(buffer))) > 0) output_text.append(buffer, static_cast<std::size_t>(count));
        close(output[0]);
        const auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - dispatched_at).count();
        bool original_restored = XGetSelectionOwner(display, original.clipboard) == original.window;
        if (!takeover) {
            const Selection verifier = OpenSelection(display);
            const auto restored = Convert(verifier, verifier.utf8, 1024, deadline);
            original_restored = restored.has_value() && restored->bytes == saved;
        }
        XSetSelectionOwner(display, original.clipboard, original.window, CurrentTime);
        XSync(display, False); XCloseDisplay(display);
        RequireSequence(exited && WIFEXITED(status) && WEXITSTATUS(status) == 0, "paste helper failed");
        if (takeover) {
            RequireSequence(taken_over && correct == 1 && output_text.find("\"restored\":\"ownership_changed\"") != std::string::npos, "later clipboard owner was overwritten");
        } else if (flood) {
            RequireSequence(correct == 1 && elapsed < 2500, "continuous requests defeated the independent service deadline");
        } else {
            RequireSequence(received == formats.size() && correct == 3 && unsupported == 1, "delayed paste conversions did not all receive the same temporary offer");
        }
        RequireSequence(original_restored, "original clipboard was not restored after the complete sequence");
        std::cout << "PASS: " << (takeover ? "later clipboard owner retained" : flood ? "continuous requests respect independent service deadline" : "delayed three-format paste, unsupported conversion response, original clipboard restoration") << '\n';
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n'; return 1;
    }
}
