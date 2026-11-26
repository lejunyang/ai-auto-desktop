// Included inside the helper's private namespace after the shared X11 utilities.
// A separate command keeps the existing single-PID gesture proof unchanged.
constexpr unsigned kCrossDragSegments = 12;
constexpr long long kCrossDragMaxPixels = 4096;

struct CrossEndpoint {
    unsigned long pid = 0;
    WindowBinding window;
    int x = 0, y = 0;
};

// A managed desktop is checked against the complete EWMH list.
bool CrossManagedClient(const Ewmh &wm, Window window) {
    if (!wm.present) return false;
    const auto clients = ReadProperty32(wm.display, wm.root, wm.client_list, XA_WINDOW, 4096);
    return clients && clients->present &&
        std::find(clients->values.begin(), clients->values.end(), window) != clients->values.end();
}

std::optional<WindowGeometryProof> CrossWindowGeometry(const Ewmh &wm, const CrossEndpoint &end) {
    if (!PidBelongsToCurrentUser(end.pid) ||
        !BoundClientViewable(wm.display, end.pid, wm.pid, end.window.id)) return std::nullopt;
    if (!end.window.proof.empty()) return RecheckGeometryProof(wm, end.pid, end.window);

    // Legacy requests bind only the exact client rect. Verified WM frames
    // can still be part of the existing cross-resource route.
    const auto bounds = RootGeometry(wm.display, end.window.id);
    if (!bounds || !GeometryMatches(*bounds, end.window.bounds, 0)) return std::nullopt;
    if (CrossManagedClient(wm, end.window.id)) {
        if (auto geometry = ReadClientGeometry(wm, end.window.id, end.pid, end.window.bounds)) {
            return geometry;
        }
    }
    return WindowGeometryProof{end.pid, end.window.id, end.window.bounds, end.window.id,
                               *bounds, {}, {}, {}};
}

bool CrossManagedDesktop(const Ewmh &wm, Window window, Atom type_atom, Atom desktop_atom) {
    if (!Contains(AtomList(wm.display, window, type_atom), desktop_atom)) return false;
    const auto pid = OwnPid(wm.display, window, wm.pid);
    if (!pid.has_value() || !PidBelongsToCurrentUser(*pid)) return false;
    XWindowAttributes attributes{};
    return XGetWindowAttributes(wm.display, window, &attributes) && !attributes.override_redirect
        && CrossManagedClient(wm,window);
}

using CrossGeometry = std::array<WindowGeometryProof, 2>;

bool CrossPointAllowed(Display *display, int x, int y, const CrossEndpoint &source,
                       const CrossEndpoint &destination, const Ewmh &wm, Atom type_atom, Atom desktop_atom,
                       const CrossGeometry &geometry) {
    Window current = DefaultRootWindow(display);
    std::optional<unsigned long> allowed_pid;
    const FrameOwner *frame_owner = nullptr;
    for (int depth = 0; depth < 64; ++depth) {
        Window child = None; int local_x = 0, local_y = 0;
        g_x_error = false;
        const bool translated = XTranslateCoordinates(display, DefaultRootWindow(display), current,
                                                       x, y, &local_x, &local_y, &child) != 0;
        XSync(display, False);
        if (!translated || g_x_error) return false;
        if (child == None) {
            if (current == DefaultRootWindow(display)) return true;
            if (!allowed_pid && frame_owner &&
                (current & ~frame_owner->mask) == (frame_owner->identity & ~frame_owner->mask)) return true;
            return allowed_pid.has_value() && WindowPid(display, current, wm.pid) == allowed_pid;
        }
        XWindowAttributes attributes{};
        if (!XGetWindowAttributes(display, child, &attributes) || attributes.override_redirect) return false;
        for (const auto &proof : geometry) {
            if (proof.wm && std::any_of(proof.frames.begin(), proof.frames.end(),
                                       [&](const FrameGeometry &frame) { return frame.id == child; })) {
                frame_owner = &*proof.wm;
            }
        }
        if (child == source.window.id) allowed_pid = source.pid;
        else if (child == destination.window.id) allowed_pid = destination.pid;
        else if (!allowed_pid.has_value() && CrossManagedDesktop(wm, child, type_atom, desktop_atom))
            allowed_pid = OwnPid(display, child, wm.pid);
        current = child;
    }
    return false;
}

std::string_view CrossRouteProof(Display *display, int from_x, int from_y, int to_x, int to_y,
                                 const CrossEndpoint &source, const CrossEndpoint &destination,
                                 const Ewmh &wm, Atom type_atom, Atom desktop_atom,
                                 const CrossGeometry &geometry, std::uint64_t deadline) {
    const long long dx = static_cast<long long>(to_x) - from_x;
    const long long dy = static_cast<long long>(to_y) - from_y;
    const long long steps = std::max(std::abs(dx), std::abs(dy));
    if (steps > kCrossDragMaxPixels) return "cross_drag_route_too_long";
    for (long long index = 0; index <= std::max(steps, 1LL); ++index) {
        if (DeadlineExpired(deadline)) return "deadline_exceeded";
        const auto denominator = std::max(steps, 1LL);
        const int x = from_x + static_cast<int>(dx * index / denominator);
        const int y = from_y + static_cast<int>(dy * index / denominator);
        if (!CrossPointAllowed(display, x, y, source, destination, wm, type_atom, desktop_atom,geometry))
            return "cross_drag_route_obstructed";
    }
    return {};
}

int RunDragBetween(int argc, char **argv) {
    const auto pairs = ParsePairs(argc, argv, {"source-pid", "destination-pid", "source-x", "source-y",
        "destination-x", "destination-y", "source-window-id", "source-window-x", "source-window-y",
        "source-window-width", "source-window-height", "destination-window-id", "destination-window-x",
        "destination-window-y", "destination-window-width", "destination-window-height",
        "source-window-proof", "destination-window-proof", "button",
        "modifiers", "hold-ms", "path-token", "deadline-monotonic-ns"});
    auto invalid = [] { EmitError("invalid_arguments", "arguments", false); return kExitInvalid; };
    if (!pairs) return invalid();
    CrossEndpoint source, destination;
    auto parse_end = [&](const std::string &prefix, CrossEndpoint *end) {
        const auto pid = ParseUnsigned(Pair(*pairs, prefix + "-pid").value_or(""));
        const auto id = ParseUnsigned(Pair(*pairs, prefix + "-window-id").value_or(""));
        if (!pid || *pid == 0 || *pid > INT_MAX || !id || *id == 0 || *id > 0xffffffffUL) return false;
        end->pid = static_cast<unsigned long>(*pid); end->window.id = static_cast<Window>(*id);
        if (const auto proof = Pair(*pairs, prefix + "-window-proof")) {
            if (proof->empty() || proof->size() > kMaxGeometryProofBytes) return false;
            end->window.proof = *proof;
        }
        return ParseInt(Pair(*pairs,prefix+"-x").value_or(""),&end->x)
            && ParseInt(Pair(*pairs,prefix+"-y").value_or(""),&end->y)
            && ParseInt(Pair(*pairs,prefix+"-window-x").value_or(""),&end->window.bounds.x)
            && ParseInt(Pair(*pairs,prefix+"-window-y").value_or(""),&end->window.bounds.y)
            && ParseInt(Pair(*pairs,prefix+"-window-width").value_or(""),&end->window.bounds.width)
            && ParseInt(Pair(*pairs,prefix+"-window-height").value_or(""),&end->window.bounds.height)
            && ValidGeometry(end->window.bounds);
    };
    const auto deadline = ParseUnsigned(Pair(*pairs, "deadline-monotonic-ns").value_or(""));
    const auto button = BoundedUnsigned(Pair(*pairs,"button"),1,3);
    const auto hold = Pair(*pairs,"hold-ms");
    const auto hold_ms = hold ? BoundedUnsigned(hold,1,5000) : std::optional<unsigned>(0);
    GestureRequest request;
    request.path_token = Pair(*pairs,"path-token").value_or("");
    if (!parse_end("source",&source) || !parse_end("destination",&destination)
        || source.window.proof.empty() != destination.window.proof.empty()
        || !deadline || !button || !hold_ms
        || request.path_token.size()!=32 || !std::all_of(request.path_token.begin(),request.path_token.end(),
            [](char c){return (c>='0'&&c<='9')||(c>='a'&&c<='f');})) return invalid();
    std::vector<std::string> modifier_names;
    if (const auto raw = Pair(*pairs,"modifiers")) {
        if (raw->empty() || raw->size()>64) return invalid();
        std::size_t start=0;
        while(start<=raw->size()) {
            const auto end=std::min(raw->find(',',start),raw->size()); const auto name=raw->substr(start,end-start);
            if ((name!="ctrl"&&name!="shift"&&name!="alt"&&name!="meta")
                || std::find(modifier_names.begin(),modifier_names.end(),name)!=modifier_names.end()) return invalid();
            modifier_names.push_back(name); start=end+1;
        }
    }
    if (std::max(std::abs(static_cast<long long>(destination.x)-source.x),
                 std::abs(static_cast<long long>(destination.y)-source.y)) > kCrossDragMaxPixels) return invalid();
    InputCancellation cancellation;
    int exit_code=0;
    Display *display=OpenQualifiedDisplay(std::nullopt,&exit_code);
    if (!display) return exit_code;
    int event_base=0,error_base=0,major=0,minor=0;
    if (!XTestQueryExtension(display,&event_base,&error_base,&major,&minor)) {
        XCloseDisplay(display); EmitError("xtest_unavailable","x11_preflight",false); return kExitUnavailable;
    }
    const Ewmh wm=LoadEwmh(display);
    const Atom type_atom=XInternAtom(display,"_NET_WM_WINDOW_TYPE",False);
    const Atom desktop_atom=XInternAtom(display,"_NET_WM_WINDOW_TYPE_DESKTOP",False);
    CrossGeometry geometry{};
    std::vector<KeyCode> modifier_codes, owned_modifiers;
    for (const auto &name:modifier_names) {
        const auto code=XKeysymToKeycode(display,name=="meta"?XK_Super_L:KeyNamed(name));
        if (!code) { XCloseDisplay(display); EmitError("modifier_unavailable_or_held","key_preflight",false); return kExitTargetMismatch; }
        modifier_codes.push_back(code);
    }
    bool dispatched=false, owned_button=false;
    unsigned long event_count=0;
    int current_x=source.x,current_y=source.y;
    auto release=[&] {
        if (owned_button) { XTestFakeButtonEvent(display,*button,False,1); owned_button=false; }
        for(auto it=owned_modifiers.rbegin();it!=owned_modifiers.rend();++it) XTestFakeKeyEvent(display,*it,False,1);
        owned_modifiers.clear(); XSync(display,False);
    };
    auto fail=[&](std::string_view reason,std::string_view phase) {
        release(); XCloseDisplay(display); EmitError(reason,phase,dispatched);
        return dispatched?kExitUnknownEffect:kExitTargetMismatch;
    };
    auto verify=[&]() -> std::string_view {
        if (DeadlineExpired(*deadline)) return "deadline_exceeded";
        auto source_geometry = CrossWindowGeometry(wm, source);
        auto destination_geometry = CrossWindowGeometry(wm, destination);
        if (!source_geometry || !destination_geometry) return "target_window_changed";
        geometry = {std::move(*source_geometry), std::move(*destination_geometry)};
        if (!PointInClient(display, source.x, source.y, source.window.id) ||
            !PointInClient(display, destination.x, destination.y, destination.window.id)) return "target_point_mismatch";
        if (!modifier_codes.empty() && (!FocusBelongsToPid(display, source.pid, wm.pid) ||
                                        !FocusInClient(display, source.window.id))) return "focus_owner_changed";
        if (!CrossPointAllowed(display,current_x,current_y,source,destination,wm,type_atom,desktop_atom,geometry)) return "cross_drag_route_obstructed";
        if (owned_button) {
            Window root=None,child=None; int rx=0,ry=0,wx=0,wy=0; unsigned mask=0;
            if (!XQueryPointer(display,DefaultRootWindow(display),&root,&child,&rx,&ry,&wx,&wy,&mask)
                || rx!=current_x||ry!=current_y) return "pointer_position_changed";
        }
        return {};
    };
    auto unheld=[&](bool allow_owned=false) {
        char keys[32]{}; XQueryKeymap(display,keys);
        if (allow_owned) for(const auto code:owned_modifiers) keys[code/8]&=static_cast<char>(~(1<<(code%8)));
        Window root=None,child=None; int rx=0,ry=0,wx=0,wy=0; unsigned mask=0;
        return XQueryPointer(display,DefaultRootWindow(display),&root,&child,&rx,&ry,&wx,&wy,&mask)
            && !std::any_of(std::begin(keys),std::end(keys),[](char k){return k!=0;})
            && !(mask&(Button1Mask|Button2Mask|Button3Mask|Button4Mask|Button5Mask));
    };
    if (const auto problem=verify();!problem.empty()) return fail(problem,"target_preflight");
    if (!unheld()) return fail("input_already_held","input_preflight");
    if (const auto problem=CrossRouteProof(display,source.x,source.y,destination.x,destination.y,
        source,destination,wm,type_atom,desktop_atom,geometry,*deadline);!problem.empty()) return fail(problem,"route_preflight");
    if (const auto problem=ValidatePathSegment(request,0,*deadline,verify);!problem.empty()) return fail(problem,"path_validation");
    if (!unheld()) return fail("input_already_held","input_preflight");
    if (const auto problem=verify();!problem.empty()) return fail(problem,"target_preflight");
    std::cout << "{\"event\":\"dispatch_started\"}" << std::endl; dispatched=true;
    auto send=[&](bool ok) { ++event_count; XSync(display,False); return ok&&!g_x_error; };
    g_x_error=false;
    if (!send(XTestFakeMotionEvent(display,-1,source.x,source.y,1)!=0)) return fail("x11_error","source_dispatch");
    if (const auto problem=ValidatePathSegment(request,1,*deadline,verify);!problem.empty()) return fail(problem,"hover_validation");
    if (!unheld()) return fail("input_already_held","modifier_preflight");
    for(const auto code:modifier_codes) {
        if (const auto problem=verify();!problem.empty()) return fail(problem,"modifier_dispatch");
        owned_modifiers.push_back(code); g_x_error=false;
        if (!send(XTestFakeKeyEvent(display,code,True,1)!=0)) return fail("x11_error","modifier_dispatch");
    }
    if (const auto problem=ValidatePathSegment(request,2,*deadline,verify);!problem.empty()) return fail(problem,"modifier_validation");
    if (!unheld(true)) return fail("input_already_held","button_preflight");
    if (const auto problem=verify();!problem.empty()) return fail(problem,"source_dispatch");
    g_x_error=false;
    owned_button=true;
    if (!send(XTestFakeButtonEvent(display,*button,True,1)!=0)) return fail("x11_error","source_dispatch");
    const auto hold_until=std::chrono::steady_clock::now()+std::chrono::milliseconds(*hold_ms);
    while(std::chrono::steady_clock::now()<hold_until) {
        if(const auto problem=verify();!problem.empty()) return fail(problem,"input_hold");
        usleep(5000);
    }
    for(unsigned segment=1;segment<=kCrossDragSegments;++segment) {
        if(const auto problem=ValidatePathSegment(request,segment+2,*deadline,verify);!problem.empty()) return fail(problem,"path_validation");
        const int x=source.x+static_cast<int>((static_cast<long long>(destination.x)-source.x)*segment/kCrossDragSegments);
        const int y=source.y+static_cast<int>((static_cast<long long>(destination.y)-source.y)*segment/kCrossDragSegments);
        if(const auto problem=CrossRouteProof(display,current_x,current_y,x,y,source,destination,wm,type_atom,desktop_atom,geometry,*deadline);!problem.empty()) return fail(problem,"route_validation");
        if(const auto problem=verify();!problem.empty()) return fail(problem,"pointer_dispatch");
        g_x_error=false;
        if (!send(XTestFakeMotionEvent(display,-1,x,y,1)!=0)) return fail("x11_error","pointer_dispatch");
        current_x=x;current_y=y;usleep(15000);
    }
    if(const auto problem=ValidatePathSegment(request,kCrossDragSegments+3,*deadline,verify);!problem.empty()) return fail(problem,"release_validation");
    if(const auto problem=verify();!problem.empty()) return fail(problem,"release_validation");
    g_x_error=false;
    if(!send(XTestFakeButtonEvent(display,*button,False,1)!=0)) return fail("x11_error","release_dispatch");
    owned_button=false;
    for(auto it=owned_modifiers.rbegin();it!=owned_modifiers.rend();++it) {
        if(!send(XTestFakeKeyEvent(display,*it,False,1)!=0)) return fail("x11_error","release_dispatch");
    }
    owned_modifiers.clear(); XCloseDisplay(display);
    std::cout << "{\"ok\":true,\"dispatch_started\":true,\"events\":" << event_count << "}" << std::endl;
    return 0;
}
