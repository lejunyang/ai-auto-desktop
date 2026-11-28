// Included by x11_xtest_helper.cpp after the common X11 guards and pipe codec.
struct TimedPointer {
    int x = 0;
    int y = 0;
    unsigned mask = 0;
};

std::optional<TimedPointer> TimedPointerState(Display *display) {
    Window root = None, child = None;
    int wx = 0, wy = 0;
    TimedPointer pointer;
    if (!XQueryPointer(display, DefaultRootWindow(display), &root, &child,
                       &pointer.x, &pointer.y, &wx, &wy, &pointer.mask)) return std::nullopt;
    return pointer;
}

struct TimedClickPlan {
    struct Move { unsigned at; int x; int y; bool overshoot; };
    int start_x, start_y, x, y, left, top, right, bottom;
    unsigned settle, hold;
    std::vector<Move> moves;

    bool HitsButton(int px, int py) const {
        return px >= left && px <= right && py >= top && py <= bottom;
    }

    static std::optional<TimedClickPlan> Parse(const std::string &body) {
        std::istringstream input(body);
        TimedClickPlan plan{};
        unsigned count = 0;
        if (!(input >> plan.start_x >> plan.start_y >> plan.x >> plan.y
                    >> plan.left >> plan.top >> plan.right >> plan.bottom
                    >> plan.settle >> plan.hold >> count) ||
            count > 165 || plan.settle < 12 || plan.settle > 80 ||
            plan.hold == 0 || plan.hold > 5000 || !plan.HitsButton(plan.x, plan.y)) return std::nullopt;
        unsigned previous = 0;
        for (unsigned i = 0; i < count; ++i) {
            Move move{};
            int overshoot = 0;
            if (!(input >> move.at >> move.x >> move.y >> overshoot) ||
                move.at <= previous || move.at > 1300 || (overshoot != 0 && overshoot != 1) ||
                (overshoot && !plan.HitsButton(move.x, move.y))) return std::nullopt;
            move.overshoot = overshoot != 0;
            plan.moves.push_back(move);
            previous = move.at;
        }
        int last_x = plan.moves.empty() ? plan.start_x : plan.moves.back().x;
        int last_y = plan.moves.empty() ? plan.start_y : plan.moves.back().y;
        std::string extra;
        if (last_x != plan.x || last_y != plan.y || (input >> extra)) return std::nullopt;
        return plan;
    }
};

int RunTimedClick(Display *display, unsigned long pid, Atom pid_atom,
                  const WindowBinding &binding, std::uint64_t deadline,
                  std::string_view token, const TimedClickPlan &plan) {
    constexpr unsigned buttons = Button1Mask | Button2Mask | Button3Mask | Button4Mask | Button5Mask;
    constexpr unsigned modifiers = ShiftMask | ControlMask | Mod1Mask | Mod3Mask | Mod4Mask | Mod5Mask;
    InputProgress progress("pointer_event", plan.moves.size() + 2);
    TimedClickProgress click;
    progress.proven_window = binding.id;
    bool dispatched = false;
    int x = plan.start_x, y = plan.start_y;
    std::vector<std::pair<int, int>> escape;
    auto hit = [&](int px, int py) {
        return px >= 0 && py >= 0 && px < DisplayWidth(display, DefaultScreen(display)) &&
            py < DisplayHeight(display, DefaultScreen(display)) && PointInClient(display, px, py, binding.id);
    };
    auto live = [&](bool pressed) -> std::string_view {
        if (DeadlineExpired(deadline)) return "deadline_exceeded";
        if (!WindowBindingMatches(display, pid, pid_atom, binding, 0) ||
            !FocusInClient(display, binding.id)) return "focus_owner_changed";
        if (!hit(plan.x, plan.y)) return "target_point_mismatch";
        const auto pointer = TimedPointerState(display);
        if (!pointer || pointer->x != x || pointer->y != y) return "pointer_position_changed";
        if ((pointer->mask & (buttons | modifiers)) != (pressed ? Button1Mask : 0))
            return "pointer_state_changed";
        return {};
    };
    auto finish = [&](std::string_view problem) {
        if (click.down_attempted && !click.up_confirmed) {
            // An ordinary release over the still-armed button can activate it.
            // Prefer a verified point outside the button in its own client.
            // Even if every escape is now obstructed, release the owned
            // button and leave the effect unknown.
            (void) AtomicBurst(display, []() -> std::string_view { return {}; }, [&] {
                const auto pointer = TimedPointerState(display);
                if (pointer && (pointer->mask & Button1Mask) == 0) return true;
                if (pointer && plan.HitsButton(pointer->x, pointer->y) &&
                    WindowBindingMatches(display, pid, pid_atom, binding, 0)) {
                    for (const auto &[ex, ey] : escape) {
                        if (!hit(ex, ey)) continue;
                        click.cleanup_motion = XTestFakeMotionEvent(display, -1, ex, ey, 0) != 0;
                        if (click.cleanup_motion) XSync(display, False);
                        break;
                    }
                }
                return XTestFakeButtonEvent(display, 1, False, 0) != 0;
            }, [] {});
        }
        progress.Emit(display, &click);
        XCloseDisplay(display);
        if (!problem.empty()) {
            EmitError(problem, "timed_click", dispatched);
            return dispatched ? kExitUnknownEffect :
                (problem == "deadline_exceeded" ? kExitTimeoutBeforeDispatch : kExitTargetMismatch);
        }
        std::cout << "{\"ok\":true,\"dispatch_started\":true,\"events\":"
                  << progress.delivered << "}" << std::endl;
        return 0;
    };
    if (const auto problem = live(false); !problem.empty()) return finish(problem);
    const auto client = RootGeometry(display, binding.id);
    if (!client || !hit(x, y)) return finish("target_point_mismatch");
    // Inset the four corners so the cleanup move stays clear of frame edges.
    for (const int ex : {client->x + 1, client->x + client->width - 2}) {
        for (const int ey : {client->y + 1, client->y + client->height - 2}) {
            if (!plan.HitsButton(ex, ey) && hit(ex, ey)) escape.emplace_back(ex, ey);
        }
    }
    if (escape.empty()) return finish("no_timed_click_escape");
    unsigned char mapping[256]{};
    if (XGetPointerMapping(display, mapping, sizeof(mapping)) < 1 || mapping[0] != 1)
        return finish("unsupported_pointer_mapping");
    for (const auto &move : plan.moves)
        if (!hit(move.x, move.y)) return finish("target_point_mismatch");

    using Clock = std::chrono::steady_clock;
    using Ms = std::chrono::milliseconds;
    auto wait = [&](Clock::time_point until, bool pressed) -> std::string_view {
        for (;;) {
            if (const auto problem = live(pressed); !problem.empty()) return problem;
            const auto now = Clock::now();
            if (now >= until) return {};
            const auto delay = std::min(until - now, Clock::duration(Ms(5)));
            usleep(static_cast<useconds_t>(std::chrono::duration_cast<std::chrono::microseconds>(delay).count()));
        }
    };
    std::size_t phase = 0;
    auto revalidate = [&](bool pressed) {
        return ValidateSegment(token, phase++, deadline, [&] { return live(pressed); }, 1, true);
    };
    auto inject = [&](bool pressed, const auto &verify, const auto &emit) {
        return AtomicBurst(display, [&]() -> std::string_view {
            if (const auto problem = live(pressed); !problem.empty()) return problem;
            return verify();
        }, [&] {
            if (DeadlineExpired(deadline)) return false;
            MarkDispatchStarted(dispatched);
            return emit();
        }, [] {});
    };
    if (const auto problem = revalidate(false); !problem.empty()) return finish(problem);
    const auto duration = Ms((plan.moves.empty() ? 0 : plan.moves.back().at) + plan.settle + plan.hold);
    const auto ready_at = Clock::now() + duration;
    if (static_cast<std::uint64_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(
            ready_at.time_since_epoch()).count()) >= deadline) return finish("deadline_exceeded");
    const auto movement_start = Clock::now();
    auto previous = movement_start;
    bool overshoot_checked = false;
    for (const auto &move : plan.moves) {
        if (move.overshoot && !overshoot_checked) {
            if (const auto problem = revalidate(false); !problem.empty()) return finish(problem);
            overshoot_checked = true;
        }
        // Use the plan's clock so round trips do not add their latency to
        // every interval. Keep a minimum spacing after an unusually slow
        // check instead of flushing queued motion events in a burst.
        const auto due = std::max(movement_start + Ms(move.at), previous + Ms(4));
        if (const auto problem = wait(due, false); !problem.empty()) return finish(problem);
        const auto sent = inject(false, [&]() -> std::string_view {
            return hit(move.x, move.y) ? "" : "target_point_mismatch";
        }, [&] { return XTestFakeMotionEvent(display, -1, move.x, move.y, 0) != 0; });
        if (!sent.empty()) return finish(sent);
        x = move.x;
        y = move.y;
        ++progress.delivered;
        ++click.moves;
        previous = Clock::now();
    }
    if (const auto problem = revalidate(false); !problem.empty()) return finish(problem);
    if (const auto problem = wait(previous + Ms(plan.settle), false); !problem.empty()) return finish(problem);
    const auto allowed = []() -> std::string_view { return {}; };
    const auto down = inject(false, [&]() -> std::string_view {
        return std::any_of(escape.begin(), escape.end(), [&](auto p) { return hit(p.first, p.second); })
            ? "" : "no_timed_click_escape";
    }, [&] {
        click.down_attempted = true;
        progress.attempted_buttons = Button1Mask;
        return XTestFakeButtonEvent(display, 1, True, 0) != 0;
    });
    if (!down.empty()) return finish(down);
    ++progress.delivered;

    const auto held_since = Clock::now();
    // timed-click-v1: ask Rust for fresh element proof every 125 ms
    // while held, and again immediately before the normal release.
    for (unsigned at = 125; at < plan.hold; at += 125) {
        if (const auto problem = wait(held_since + Ms(at), true); !problem.empty()) return finish(problem);
        if (const auto problem = revalidate(true); !problem.empty()) return finish(problem);
    }
    if (const auto problem = wait(held_since + Ms(plan.hold), true); !problem.empty()) return finish(problem);
    if (const auto problem = revalidate(true); !problem.empty()) return finish(problem);
    const auto up = inject(true, allowed, [&] { return XTestFakeButtonEvent(display, 1, False, 0) != 0; });
    if (!up.empty()) return finish(up);
    const auto released = TimedPointerState(display);
    if (!released || (released->mask & Button1Mask) != 0) return finish("release_unconfirmed");
    click.up_confirmed = true;
    ++progress.delivered;
    return finish({});
}

int TimedClickCommand(std::string_view command, int argc, char **argv,
                      const std::optional<WindowBinding> &binding, std::string_view token) {
    const auto args = ParsePairs(argc, argv, {"expected-pid", "deadline-monotonic-ns"});
    const auto pid = args ? ParseUnsigned(Pair(*args, "expected-pid").value_or("")) : std::nullopt;
    const auto deadline = args ? ParseUnsigned(Pair(*args, "deadline-monotonic-ns").value_or("")) : std::nullopt;
    auto fail = [](std::string_view problem, int code, Display *display = nullptr) {
        if (display) XCloseDisplay(display);
        EmitError(problem, "timed_click_preflight", false);
        return code;
    };
    if (!pid || *pid == 0 || *pid > INT_MAX || !deadline || !binding || binding->proof.empty() ||
        (token.empty() != (command == "timed-click-origin")))
        return fail("invalid_arguments", kExitInvalid);
    if (!SessionIsQualified()) return fail("unsupported_session", kExitUnavailable);
    if (!PidBelongsToCurrentUser(*pid)) return fail("untrusted_process", kExitTargetMismatch);
    std::optional<TimedClickPlan> plan;
    if (!token.empty()) {
        std::string body;
        if (const auto problem = ReadTextFrame(8192, *deadline, &body); !problem.empty())
            return fail(problem, kExitInvalid);
        plan = TimedClickPlan::Parse(body);
        if (!plan) return fail("invalid_arguments", kExitInvalid);
    }
    if (DeadlineExpired(*deadline)) return fail("deadline_exceeded", kExitTimeoutBeforeDispatch);
    XSetErrorHandler(HandleXError);
    Display *display = XOpenDisplay(nullptr);
    if (!display) return fail("display_unavailable", kExitUnavailable);
    int event_base = 0, error_base = 0, major = 0, minor = 0;
    if (!XTestQueryExtension(display, &event_base, &error_base, &major, &minor))
        return fail("xtest_unavailable", kExitUnavailable, display);
    const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", True);
    if (pid_atom == None || !FocusMatchesWindow(display, *pid, pid_atom, binding))
        return fail("focus_owner_changed", kExitTargetMismatch, display);
    if (plan) return RunTimedClick(display, *pid, pid_atom, *binding, *deadline, token, *plan);
    const auto pointer = TimedPointerState(display);
    if (!pointer) return fail("target_point_mismatch", kExitTargetMismatch, display);
    XCloseDisplay(display);
    std::cout << "{\"ok\":true,\"x\":" << pointer->x << ",\"y\":" << pointer->y << "}" << std::endl;
    return 0;
}
