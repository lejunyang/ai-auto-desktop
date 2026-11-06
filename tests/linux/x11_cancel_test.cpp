// Cooperative helper cancellation on a private X server. No user clipboard,
// window or input is used. Independently count delivered events and read back
// the synthetic original clipboard after a cancelled paste.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <stdexcept>

static void RequireCancel(bool ok, const char *message) {
    if (!ok) throw std::runtime_error(message);
}

int main() {
    try {
        setenv("XDG_SESSION_TYPE", "x11", 1);
        Display *display = XOpenDisplay(nullptr);
        RequireCancel(display != nullptr, "private display unavailable");
        XSetErrorHandler(HandleXError);
        const Window target = XCreateSimpleWindow(display, DefaultRootWindow(display), 20, 20, 200, 200, 0, 0, 0);
        const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", False);
        const unsigned long pid = getpid();
        XChangeProperty(display, target, pid_atom, XA_CARDINAL, 32, PropModeReplace, reinterpret_cast<const unsigned char *>(&pid), 1);
        XSelectInput(display, target, KeyPressMask | KeyReleaseMask);
        XMapWindow(display, target);
        XSetInputFocus(display, target, RevertToParent, CurrentTime);
        XSync(display, False);
        const Selection original = OpenSelection(display);
        const std::string saved = "private-original-clipboard";
        const std::vector<Item> items{{original.utf8, original.utf8, 8, saved}};
        struct Result { unsigned presses = 0; unsigned releases = 0; long elapsed_ms = 0; std::string output; };
        auto invoke = [&](std::vector<std::string> args, const std::string &text, bool global, bool cancel_clipboard_read = false) {
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); }
            const auto until_ns = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() + 5000000000LL;
            args.insert(args.begin(), "helper");
            if (!global) args.insert(args.end(), {"--expected-pid", std::to_string(pid), "--window-id", std::to_string(target), "--window-x", "20", "--window-y", "20", "--window-width", "200", "--window-height", "200"});
            args.insert(args.end(), {"--deadline-monotonic-ns", std::to_string(until_ns)});
            int input[2], output[2];
            RequireCancel(pipe(input) == 0 && pipe(output) == 0, "pipe failed");
            const pid_t worker = fork(); RequireCancel(worker >= 0, "fork failed");
            if (worker == 0) {
                close(ConnectionNumber(display));
                close(input[1]); dup2(input[0], STDIN_FILENO); close(input[0]);
                close(output[0]); dup2(output[1], STDOUT_FILENO); close(output[1]);
                std::vector<char *> raw; for (auto &arg : args) raw.push_back(arg.data());
                _exit(aad_xtest_helper_main(static_cast<int>(raw.size()), raw.data()));
            }
            close(input[0]); close(output[1]);
            if (!text.empty()) RequireCancel(write(input[1], text.data(), text.size()) == static_cast<ssize_t>(text.size()), "stdin write failed");
            close(input[1]);
            Result result{}; int status = 0; bool cancelled = false;
            auto started = std::chrono::steady_clock::now();
            auto drain = [&] {
                XSync(display, False);
                while (XPending(display)) {
                    XEvent event{}; XNextEvent(display, &event);
                    if (event.type == SelectionRequest) {
                        if (cancel_clipboard_read && !cancelled) {
                            cancelled = true;
                            started = std::chrono::steady_clock::now();
                            RequireCancel(kill(worker, SIGTERM) == 0, "cannot cancel clipboard preflight");
                        }
                        Serve(original, event.xselectionrequest, items);
                    }
                    if (event.type == KeyRelease) ++result.releases;
                    if (event.type == KeyPress) {
                        ++result.presses;
                        if (!cancelled) {
                            cancelled = true;
                            started = std::chrono::steady_clock::now();
                            RequireCancel(kill(worker, SIGTERM) == 0, "cannot cancel active helper");
                        }
                    }
                }
            };
            const auto until = started + std::chrono::seconds(5);
            for (;;) {
                drain();
                if (waitpid(worker, &status, WNOHANG) == worker) break;
                if (std::chrono::steady_clock::now() > until) { kill(worker, SIGKILL); waitpid(worker, &status, 0); throw std::runtime_error("cancel did not terminate helper"); }
                usleep(200);
            }
            drain();
            result.elapsed_ms = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - started).count();
            char bytes[4096]; ssize_t count;
            while ((count = read(output[0], bytes, sizeof(bytes))) > 0) result.output.append(bytes, static_cast<std::size_t>(count));
            close(output[0]);
            RequireCancel(cancelled && WIFEXITED(status), "helper exited without cooperative cleanup");
            RequireCancel(result.elapsed_ms < 1500, "cancel waited for original deadline");
            RequireCancel(result.presses == result.releases, "owned key prefix was not released");
            char keys[32]{}; XQueryKeymap(display, keys);
            for (char value : keys) RequireCancel(value == 0, "key remained held after cancellation");
            return result;
        };

        const auto typed = invoke({"type-text"}, std::string(1024, 'a'), false);
        RequireCancel(typed.presses > 0 && typed.presses < 1024, "type cancellation did not stop remaining strokes");
        const auto global = invoke({"global-key", "--keys", "ctrl,shift,alt,meta,f2"}, "", true);
        RequireCancel(global.presses > 0 && global.presses <= 5, "global chord did not dispatch its expected prefix");

        XSetSelectionOwner(display, original.clipboard, original.window, CurrentTime);
        XSync(display, False);
        RequireCancel(XGetSelectionOwner(display, original.clipboard) == original.window, "cannot seed private clipboard");
        const auto preflight = invoke({"paste"}, "private-replacement", false, true);
        RequireCancel(preflight.presses == 0 && XGetSelectionOwner(display, original.clipboard) == original.window, "preflight cancellation replaced original clipboard owner");
        const auto pasted = invoke({"paste"}, "private-replacement", false);
        RequireCancel(pasted.presses > 0, "paste did not reach Ctrl+V before cancellation");
        const Selection reader = OpenSelection(display);
        const auto deadline = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() + 2000000000LL;
        const auto restored = Convert(reader, reader.utf8, 1024, deadline);
        RequireCancel(restored.has_value() && restored->bytes == saved, "cancelled paste did not restore original clipboard");
        // Reclaiming the private selection also terminates the detached restorer.
        XSetSelectionOwner(display, original.clipboard, original.window, CurrentTime);
        XSync(display, False);
        XCloseDisplay(display);
        std::cout << "PASS: cancelled type stops pending strokes; global prefix releases; cancelled paste restores independently read clipboard\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n'; return 1;
    }
}
