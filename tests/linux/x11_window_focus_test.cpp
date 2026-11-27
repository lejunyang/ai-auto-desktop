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
                          std::function<void(const XEvent &)> on_event = {},
                          std::string_view text = "a", Geometry observed = {20, 20, 200, 200}) {
            XSync(display, False);
            while (XPending(display)) { XEvent event{}; XNextEvent(display, &event); }
            const auto deadline = std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch()).count() + 500000000LL;
            args.insert(args.begin(), "helper");
            args.insert(args.end(), {"--expected-pid", std::to_string(pid), "--deadline-monotonic-ns", std::to_string(deadline),
                "--window-x", std::to_string(observed.x), "--window-y", std::to_string(observed.y),
                "--window-width", std::to_string(observed.width), "--window-height", std::to_string(observed.height)});
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
            RequireWindow(write(input[1], text.data(), text.size()) == static_cast<ssize_t>(text.size()), "stdin write failed"); close(input[1]);
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
        Display *other = XOpenDisplay(nullptr);
        RequireWindow(other != nullptr, "second connection unavailable");
        XSync(display, True);
        const auto delivery = AtomicBurst(display, [&]() -> std::string_view {
            if (!FocusMatchesWindow(display, pid, pid_atom, binding)) return "focus_owner_changed";
            XSetInputFocus(other, sibling, RevertToParent, CurrentTime);
            XFlush(other);
            return {};
        }, [&] {
            return XTestFakeKeyEvent(display, f2, True, 1) &&
                   XTestFakeKeyEvent(display, f2, False, 1);
        }, [&] { XTestFakeKeyEvent(display, f2, False, 0); });
        XSync(other, False);
        Window focus = None;
        int revert = 0;
        XGetInputFocus(display, &focus, &revert);
        RequireWindow(delivery.empty() && focus == sibling, "queued focus switch did not complete");
        XEvent key{};
        RequireWindow(XCheckTypedEvent(display, KeyPress, &key) && key.xkey.window == child &&
                      !XCheckTypedEvent(display, KeyPress, &key) && released(),
                      "queued focus switch intercepted the key");
        const auto denied_burst = AtomicBurst(display, [&]() -> std::string_view {
            return FocusMatchesWindow(display, pid, pid_atom, binding) ? "" : "focus_owner_changed";
        }, [&] { return XTestFakeKeyEvent(display, f2, True, 0) != 0; }, [] {});
        RequireWindow(denied_burst == "focus_owner_changed" && released(), "rejected burst pressed a key");
        XSetInputFocus(display, child, RevertToParent, CurrentTime); XSync(display, False);
        const auto failed_burst = AtomicBurst(display, [] { return std::string_view{}; }, [&] {
            XTestFakeKeyEvent(display, f2, True, 0);
            return false;
        }, [&] { XTestFakeKeyEvent(display, f2, False, 0); });
        RequireWindow(failed_burst == "x11_error" && released(), "failed burst left a key held");
        XCloseDisplay(other);
        const auto proof = invoke({"focus-info"}, true);
        RequireWindow(proof.code == 0 && proof.output.find("\"focused\":true") != std::string::npos, "exact-window proof failed");
        const auto success = invoke({"key-chord", "--keys", "f2"});
        RequireWindow(success.code == 0 && success.presses == 1 && success.releases == 1 && released(), "bound key did not reach target child");
        auto keyboard_map = [&] {
            int first = 0, last = 0, width = 0;
            XDisplayKeycodes(display, &first, &last);
            KeySym *symbols = XGetKeyboardMapping(display, static_cast<KeyCode>(first), last - first + 1, &width);
            RequireWindow(symbols != nullptr, "keyboard map unavailable");
            std::vector<KeySym> result(symbols, symbols + (last - first + 1) * width);
            XFree(symbols);
            return result;
        };
        const auto before = keyboard_map();
        const std::string nonce = "0123456789abcdef0123456789abcdef";
        const std::string ack = "ack " + nonce + " 0\n";
        for (bool physical : {false, true}) {
            std::vector<std::string> args{"type-text", "--anchor-token", nonce};
            if (physical) args.insert(args.end(), {"--mode", "keys"});
            const std::string framed = physical ? "text 2\naB" : "text 4\na汉";
            const auto no_proof = invoke(args, false, {}, framed);
            if (no_proof.code != kExitTargetMismatch || no_proof.presses || no_proof.releases ||
                before != keyboard_map()) {
                std::cerr << "Unacknowledged " << (physical ? "keys" : "Unicode")
                          << " frame exited " << no_proof.code << " with " << no_proof.presses
                          << " presses, " << no_proof.releases << " releases, restored map="
                          << (before == keyboard_map()) << ": " << no_proof.output;
            }
            RequireWindow(no_proof.code == kExitTargetMismatch && no_proof.presses == 0 &&
                          no_proof.releases == 0 && before == keyboard_map(),
                          "typing dispatched before the first ACK");
            const auto one_proof = invoke(args, false, {}, framed + ack);
            RequireWindow(one_proof.code == kExitUnknownEffect &&
                          one_proof.presses == 1 && one_proof.releases == 1 &&
                          one_proof.output.find("\"index\":1") != std::string::npos &&
                          one_proof.output.find("\"delivered\":1,\"total\":2") != std::string::npos &&
                          one_proof.output.find("\"released\":true") != std::string::npos &&
                          before == keyboard_map(), "typing sent a stroke without its own ACK");
            const auto truncated = invoke(args, false, {}, "text 4\na");
            RequireWindow(truncated.code == kExitUnsupportedText && truncated.presses == 0 &&
                          truncated.releases == 0, "truncated text frame reached dispatch");
        }
        const std::vector<std::string> barrier_args{
            "type-text", "--anchor-token", nonce, "--text-event-barrier", "required"};
        const std::string framed = "text 4\na汉" + ack + "ack " + nonce + " 1\n";
        const auto no_protocol = invoke(barrier_args, false, {}, framed);
        RequireWindow(no_protocol.code == kExitUnavailable && no_protocol.presses == 0 &&
                      no_protocol.output.find("text_event_barrier_unavailable") != std::string::npos,
                      "typing dispatched without a client event processing protocol");
        Atom ping = XInternAtom(display, "_NET_WM_PING", False);
        RequireWindow(XSetWMProtocols(display, target, &ping, 1), "could not advertise ping protocol");
        const auto wrong_reply = invoke(barrier_args, false, [&](const XEvent &event) {
            if (event.type != ClientMessage || event.xclient.window != target) return;
            XEvent reply = event;
            reply.xclient.window = root;
            ++reply.xclient.data.l[1];
            XSendEvent(display, root, False, SubstructureNotifyMask, &reply);
            XFlush(display);
        }, framed);
        RequireWindow(wrong_reply.code == kExitUnknownEffect && wrong_reply.presses == 1 &&
                      wrong_reply.releases == 1 && released() && before == keyboard_map() &&
                      wrong_reply.output.find("text_stroke_unconfirmed") != std::string::npos &&
                      wrong_reply.output.find("\"delivered\":1,\"total\":2") != std::string::npos &&
                      wrong_reply.output.find("\"index\":1") == std::string::npos,
                      "unmatched client reply authorized another stroke or left input state changed");
        const auto barrier = KeyboardEventBarrier::Open(display, target);
        RequireWindow(barrier.has_value(), "could not open window reply protocol");
        for (std::uint32_t time : {0x7fffffffu, 0x80000000u, 0xffffffffu}) {
            XClientMessageEvent sent{};
            sent.type = ClientMessage;
            sent.window = target;
            sent.message_type = barrier->protocols;
            sent.format = 32;
            sent.data.l[0] = static_cast<long>(ping);
            sent.data.l[1] = static_cast<long>(time);
            sent.data.l[2] = static_cast<long>(target);
            for (bool matching : {true, false}) {
                XEvent reply{};
                reply.xclient = sent;
                reply.xclient.window = root;
                if (!matching) reply.xclient.data.l[1] ^= 1;
                const unsigned long request = NextRequest(display);
                RequireWindow(XSendEvent(display, root, False, SubstructureNotifyMask, &reply),
                              "could not send client reply");
                XSync(display, False);
                XEvent decoded{};
                RequireWindow(XCheckTypedWindowEvent(display, root, ClientMessage, &decoded),
                              "client reply missing after Xlib round-trip");
                if (barrier->MatchesReply(decoded, sent, request) != matching) {
                    std::cerr << "Client reply timestamp sent as " << sent.data.l[1]
                              << ", decoded as " << decoded.xclient.data.l[1] << '\n';
                    throw std::runtime_error("client reply did not match its 32-bit timestamp");
                }
            }
        }
        unsigned wrong_window = 0;
        bool switched = false;
        const auto interrupted = invoke({"type-text"}, false, [&](const XEvent &event) {
            if (event.type == KeyPress && event.xkey.window == sibling) ++wrong_window;
            if (event.type == KeyRelease && event.xkey.window == child && !switched) {
                switched = true;
                XSetInputFocus(display, sibling, RevertToParent, CurrentTime);
                XSync(display, False);
            }
        }, "a汉Bcd");
        RequireWindow(switched && interrupted.code == kExitUnknownEffect && wrong_window == 0 &&
                      interrupted.output.find("focus_owner_changed") != std::string::npos &&
                      interrupted.output.find("\"delivered\":1,\"total\":5,\"proven_window\":" +
                                              std::to_string(target) + ",\"released\":true") != std::string::npos,
                      "Unicode typing reached the other window after focus changed");
        char keys[32]{};
        XQueryKeymap(display, keys);
        RequireWindow(std::none_of(std::begin(keys), std::end(keys), [](char byte) { return byte != 0; }) &&
                      before == keyboard_map(), "interrupted typing left keys held or a changed map");
        XSetInputFocus(display, child, RevertToParent, CurrentTime); XSync(display, False);
        const auto recovered = invoke({"type-text"}, false, {}, "B汉");
        RequireWindow(recovered.code == 0 && recovered.presses == 3 && recovered.releases == 3 &&
                      before == keyboard_map(), "Unicode typing did not recover after refocus");
        bool changed = false;
        const auto changed_focus = invoke({"key-chord", "--keys", "ctrl,f2", "--hold-ms", "200"}, false, [&](const XEvent &event) {
            if (event.type == KeyPress && event.xkey.keycode == f2 && !changed) {
                changed = true; XSetInputFocus(display, sibling, RevertToParent, CurrentTime); XSync(display, False);
            }
        });
        RequireWindow(changed && changed_focus.code == kExitUnknownEffect && released() &&
                      changed_focus.output.find("\"delivered\":1,\"total\":1,\"proven_window\":" +
                                                std::to_string(target) + ",\"released\":true") != std::string::npos,
                      "same-PID focus change left owned keys held or lost chord progress");
        XSetInputFocus(display, child, RevertToParent, CurrentTime); XSync(display, False);
        changed = false;
        const auto moved = invoke({"key-chord", "--keys", "f2", "--hold-ms", "200"}, false, [&](const XEvent &event) {
            if (event.type == KeyPress && !changed) { changed = true; XMoveWindow(display, target, 30, 20); XSync(display, False); }
        });
        RequireWindow(changed && moved.code == kExitUnknownEffect && released(), "window movement left owned key held");
        XUnmapWindow(display, sibling); XSync(display, False);
        const auto moved_proof = invoke({"focus-info"}, true);
        RequireWindow(moved_proof.code == 0 && moved_proof.output.find("\"focused\":false") != std::string::npos, "single-window geometry mismatch accepted");

        XMoveWindow(display, target, 20, 20);
        Display *wm = XOpenDisplay(nullptr);
        RequireWindow(wm != nullptr, "private WM connection unavailable");
        const Atom wm_check = XInternAtom(display, "_NET_SUPPORTING_WM_CHECK", False);
        const Atom client_list = XInternAtom(display, "_NET_CLIENT_LIST", False);
        const Atom frame_extents = XInternAtom(display, "_NET_FRAME_EXTENTS", False);
        const Window identity = XCreateSimpleWindow(wm, root, 0, 0, 1, 1, 0, 0, 0);
        const Window frame = XCreateSimpleWindow(wm, root, 15, 10, 210, 215, 0, 0, 0);
        auto property = [](Display *connection, Window window, Atom name, Atom type,
                           const unsigned long *values, int count) {
            XChangeProperty(connection, window, name, type, 32, PropModeReplace,
                            reinterpret_cast<const unsigned char *>(values), count);
        };
        property(wm, root, wm_check, XA_WINDOW, &identity, 1);
        property(wm, identity, wm_check, XA_WINDOW, &identity, 1);
        property(wm, identity, pid_atom, XA_CARDINAL, &pid, 1);
        XMapWindow(wm, frame);
        XSync(wm, False);
        XReparentWindow(display, target, frame, 5, 10);
        XMapWindow(display, target);
        XSetInputFocus(display, child, RevertToParent, CurrentTime);
        const unsigned long extents[] = {5, 5, 10, 5};
        property(display, target, frame_extents, XA_CARDINAL, extents, 4);
        const Window listed[] = {target, target};
        property(display, root, client_list, XA_WINDOW, listed, 2);
        XSync(display, False);
        const Geometry outer{15, 10, 210, 215};
        auto geometry_proof = [&](const Geometry &bounds) {
            return FindGeometryProof(LoadEwmh(display), pid, bounds);
        };
        auto decorated = geometry_proof(outer);
        RequireWindow(decorated && decorated->client == target && decorated->observed_window == frame &&
                      GeometryMatches(decorated->client_bounds, {20, 20, 200, 200}, 0) &&
                      decorated->frames.size() == 1 && decorated->wm->identity == identity &&
                      decorated->extents == FrameExtents{5, 5, 10, 5},
                      "trusted frame did not bind to unique real client");
        const auto client_proof = geometry_proof({21, 22, 200, 200});
        RequireWindow(client_proof && client_proof->observed_window == target &&
                      client_proof->frames.size() == 1, "client match discarded trusted frame");
        RequireWindow(!geometry_proof({23, 20, 200, 200}), "client tolerance exceeded two pixels");
        const auto helper_proof = invoke({"focus-info", "--geometry-proof", "1"}, true);
        RequireWindow(helper_proof.code == 0 && helper_proof.output.find("\"focused\":true") != std::string::npos &&
                      helper_proof.output.find("\"geometry_proof\":") != std::string::npos,
                      "helper did not emit focus and geometry proof");
        const std::string wire_proof = GeometryProofJson(*decorated);
        auto decorated_key = [&](const std::string &proof) {
            return invoke({"key-chord", "--keys", "f2", "--window-proof", proof}, false, {}, "a", outer);
        };
        const auto decorated_result = decorated_key(wire_proof);
        RequireWindow(decorated_result.code == 0 && decorated_result.presses == 1 &&
                      decorated_result.releases == 1, "frozen decoration did not allow bound input");
        auto denied = [&](const Result &result, const char *message) {
            RequireWindow(result.code == kExitTargetMismatch && result.presses == 0 &&
                          result.releases == 0 && released(), message);
        };
        denied(invoke({"key-chord", "--keys", "f2"}, false, {}, "a", outer),
               "legacy input accepted decorated observation");
        denied(decorated_key("{\"version\":2}"), "unknown or incomplete proof allowed input");
        const WindowBinding decorated_binding{target, outer, false, wire_proof};
        RequireWindow(!PointMatchesWindow(display, 18, 12, pid, pid_atom, decorated_binding) &&
                       PointMatchesWindow(display, 50, 50, pid, pid_atom, decorated_binding),
                       "proof treated WM titlebar as a client hit");
        XMoveWindow(wm, frame, outer.x + 1, outer.y);
        XSync(wm, False);
        denied(decorated_key(wire_proof), "initial lookup tolerance allowed frozen frame movement");
        XMoveWindow(wm, frame, outer.x, outer.y);
        XSync(wm, False);
        XDeleteProperty(display, target, frame_extents);
        XSync(display, False);
        const auto absent = geometry_proof(outer);
        RequireWindow(absent && !absent->extents && GeometryProofJson(*absent) != GeometryProofJson(*decorated),
                      "verified frame without extents property failed or lost change evidence");
        denied(decorated_key(wire_proof), "extents property removal allowed frozen input");
        property(display, target, frame_extents, XA_CARDINAL, extents, 3);
        XSync(display, False);
        RequireWindow(!geometry_proof(outer), "partial extents accepted");
        const unsigned long wrong_extents[] = {4, 6, 10, 5};
        property(display, target, frame_extents, XA_CARDINAL, wrong_extents, 4);
        XSync(display, False);
        RequireWindow(!geometry_proof(outer), "extents inconsistent with measured frame accepted");
        property(display, target, frame_extents, XA_CARDINAL, extents, 4);
        XMoveResizeWindow(display, sibling, outer.x, outer.y, outer.width, outer.height);
        XMapWindow(display, sibling);
        const Window ambiguous[] = {target, sibling};
        property(display, root, client_list, XA_WINDOW, ambiguous, 2);
        XSync(display, False);
        RequireWindow(!geometry_proof(outer), "client and frame candidate union was ambiguous");
        denied(decorated_key(wire_proof), "new ambiguous window allowed frozen input");
        XMoveWindow(display, sibling, 350, 20);
        XSetInputFocus(display, sibling, RevertToParent, CurrentTime);
        XSync(display, False);
        denied(decorated_key(wire_proof), "same-PID sibling accepted decorated proof");
        XSetInputFocus(display, child, RevertToParent, CurrentTime);
        std::vector<Window> excessive(4097, target);
        property(display, root, client_list, XA_WINDOW, excessive.data(), static_cast<int>(excessive.size()));
        XSync(display, False);
        RequireWindow(!geometry_proof(outer), "truncated EWMH client list accepted");
        XDeleteProperty(display, root, client_list);
        XSync(display, False);
        RequireWindow(!geometry_proof(outer), "missing EWMH client list accepted");
        property(display, root, client_list, XA_WINDOW, &target, 1);
        const Window replacement = XCreateSimpleWindow(wm, root, outer.x, outer.y, outer.width, outer.height, 0, 0, 0);
        XMapWindow(wm, replacement);
        XSync(wm, False);
        XReparentWindow(display, target, replacement, 5, 10);
        XMapWindow(display, target);
        XSetInputFocus(display, child, RevertToParent, CurrentTime);
        XSync(display, False);
        const auto replaced = geometry_proof(outer);
        RequireWindow(replaced && replaced->observed_window == replacement, "replacement frame fixture is invalid");
        denied(decorated_key(wire_proof), "equal-sized WM frame replacement accepted old proof");
        const Window fake = XCreateSimpleWindow(display, root, outer.x, outer.y, outer.width, outer.height, 0, 0, 0);
        XMapWindow(display, fake);
        XReparentWindow(display, target, fake, 5, 10);
        XMapWindow(display, target);
        XSync(display, False);
        RequireWindow(!geometry_proof(outer), "non-WM frame accepted despite matching declared extents");
        XCloseDisplay(wm);

        XReparentWindow(display, target, root, 20, 20);
        XMapWindow(display, target);
        XSetInputFocus(display, child, RevertToParent, CurrentTime);
        XSync(display, False);
        bool submitted = false;
        const auto closed = invoke(barrier_args, false, [&](const XEvent &event) {
            if (event.type == KeyRelease && event.xkey.window == child &&
                event.xkey.keycode == XKeysymToKeycode(display, XK_Return)) {
                submitted = true;
                XDestroyWindow(display, target);
                XSync(display, False);
            }
        }, "text 1\n\n" + ack);
        if (closed.code != 0) std::cerr << closed.output;
        RequireWindow(submitted && closed.code == 0 && closed.presses == 1 && closed.releases == 1 &&
                      closed.output.find("\"delivered\":1,\"total\":1,\"proven_window\":" +
                                         std::to_string(target) + ",\"released\":true") != std::string::npos,
                      "last Return closed the window but typing did not report complete dispatch");
        XCloseDisplay(display);
        std::cout << "PASS: strict window binding, atomic key cleanup, text ACK and 32-bit event barrier, mapping and hold cleanup, and decorated geometry proof\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n'; return 1;
    }
}
