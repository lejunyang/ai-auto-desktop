// Included after the shared EWMH and X11 geometry readers.
constexpr std::size_t kMaxGeometryProofBytes = 2048;
constexpr unsigned kMaxWindowFrames = 8;

struct Property32 {
    bool present = false;
    std::vector<unsigned long> values;
};

// An absent property is distinct from a malformed, partial, or failed read.
std::optional<Property32> ReadProperty32(Display *display, Window window, Atom property,
                                         Atom type, long limit) {
    Atom actual = None;
    int format = 0;
    unsigned long count = 0, after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const int status = XGetWindowProperty(display, window, property, 0, limit, False, type,
                                          &actual, &format, &count, &after, &data);
    Property32 result;
    const bool ok = status == Success && !g_x_error && after == 0 &&
        (actual == None || (actual == type && format == 32 && (count == 0 || data)));
    if (ok && actual != None) {
        result.present = true;
        if (count) {
            const auto *values = reinterpret_cast<const unsigned long *>(data);
            result.values.assign(values, values + count);
        }
    }
    if (data) XFree(data);
    if (!ok) return std::nullopt;
    return result;
}

bool ValidXid(unsigned long id) { return id != None && id <= UINT32_MAX; }

bool ValidGeometry(const Geometry &rect) {
    return rect.width > 0 && rect.height > 0 &&
        static_cast<long long>(rect.x) + rect.width <= INT_MAX &&
        static_cast<long long>(rect.y) + rect.height <= INT_MAX;
}

bool GeometryMatches(const Geometry &actual, const Geometry &observed, int tolerance) {
    return actual.width == observed.width && actual.height == observed.height &&
        std::abs(static_cast<long long>(actual.x) - observed.x) <= tolerance &&
        std::abs(static_cast<long long>(actual.y) - observed.y) <= tolerance;
}

std::optional<Window> WindowParent(const Ewmh &ewmh, Window window) {
    Window root = None, parent = None, *children = nullptr;
    unsigned count = 0;
    g_x_error = false;
    const Status ok = XQueryTree(ewmh.display, window, &root, &parent, &children, &count);
    if (children) XFree(children);
    if (!ok || g_x_error || root != ewmh.root || !ValidXid(parent) || parent == window) {
        return std::nullopt;
    }
    return parent;
}

bool CollectWindowCandidates(Display *display, Window window, unsigned depth,
                             std::vector<Window> *out) {
    Window root = None, parent = None, *children = nullptr;
    unsigned count = 0;
    g_x_error = false;
    const Status ok = XQueryTree(display, window, &root, &parent, &children, &count);
    bool complete = ok && !g_x_error && count <= 4096 - out->size();
    if (complete && count) {
        out->insert(out->end(), children, children + count);
        if (depth < 3) {
            for (unsigned i = 0; i < count && complete; ++i) {
                complete = CollectWindowCandidates(display, children[i], depth + 1, out);
            }
        }
    }
    if (children) XFree(children);
    return complete;
}

std::optional<std::vector<Window>> WindowCandidates(const Ewmh &ewmh) {
    const Atom check = XInternAtom(ewmh.display, "_NET_SUPPORTING_WM_CHECK", False);
    const auto identity = ReadProperty32(ewmh.display, ewmh.root, check, XA_WINDOW, 1);
    if (!identity) return std::nullopt;
    std::vector<Window> windows;
    if (identity->present) {
        if (!ewmh.present || identity->values.size() != 1 || !ValidXid(identity->values[0])) {
            return std::nullopt;
        }
        const auto clients = ReadProperty32(ewmh.display, ewmh.root, ewmh.client_list, XA_WINDOW, 4096);
        if (!clients || !clients->present) return std::nullopt;
        windows = clients->values;
    } else if (!CollectWindowCandidates(ewmh.display, ewmh.root, 0, &windows)) {
        return std::nullopt;
    }
    std::sort(windows.begin(), windows.end());
    windows.erase(std::unique(windows.begin(), windows.end()), windows.end());
    return windows;
}

struct FrameGeometry {
    Window id;
    Geometry bounds;
};

struct FrameOwner {
    Window identity;
    unsigned long pid;
    XID mask;
};

std::optional<XID> ResourceMask(Display *display, Window own_window);

std::optional<FrameOwner> ReadFrameOwner(const Ewmh &ewmh) {
    if (!ewmh.present) return std::nullopt;
    Display *display = ewmh.display;
    const Atom check = XInternAtom(display, "_NET_SUPPORTING_WM_CHECK", False);
    const auto identity = ReadProperty32(display, ewmh.root, check, XA_WINDOW, 1);
    if (!identity || identity->values.size() != 1 || !ValidXid(identity->values[0])) {
        return std::nullopt;
    }
    const Window window = identity->values[0];
    const auto repeated = ReadProperty32(display, window, check, XA_WINDOW, 1);
    if (!repeated || repeated->values != identity->values) return std::nullopt;
    const auto pid = ReadProperty32(display, window, ewmh.pid, XA_CARDINAL, 1);
    if (!pid || pid->values.size() != 1 || !PidBelongsToCurrentUser(pid->values[0])) {
        return std::nullopt;
    }
    const auto mask = ResourceMask(display, XAllocID(display));
    if (!mask || !ValidXid(*mask)) return std::nullopt;
    return FrameOwner{window, pid->values[0], *mask};
}

using FrameExtents = std::array<long long, 4>;

FrameExtents FramePadding(const Geometry &client, const Geometry &frame) {
    return {static_cast<long long>(client.x) - frame.x,
            static_cast<long long>(frame.x) + frame.width - client.x - client.width,
            static_cast<long long>(client.y) - frame.y,
            static_cast<long long>(frame.y) + frame.height - client.y - client.height};
}

struct WindowGeometryProof {
    unsigned long pid;
    Window client;
    Geometry observed_bounds;
    Window observed_window;
    Geometry client_bounds;
    std::vector<FrameGeometry> frames;  // closest ancestor first
    std::optional<FrameOwner> wm;
    std::optional<FrameExtents> extents;
};

std::optional<WindowGeometryProof> ReadClientGeometry(const Ewmh &ewmh, Window window,
                                                      unsigned long pid, const Geometry &observed) {
    const auto client = RootGeometry(ewmh.display, window);
    if (!client || !ValidGeometry(*client)) return std::nullopt;
    WindowGeometryProof proof{pid, window, observed, None, *client, {}, {}, {}};
    Window current = window;
    for (;;) {
        const auto parent = WindowParent(ewmh, current);
        if (!parent) return std::nullopt;
        if (*parent == ewmh.root) break;
        if (proof.frames.empty()) proof.wm = ReadFrameOwner(ewmh);
        if (!proof.wm || proof.frames.size() == kMaxWindowFrames ||
            (*parent & ~proof.wm->mask) != (proof.wm->identity & ~proof.wm->mask)) {
            return std::nullopt;
        }
        XWindowAttributes attributes{};
        g_x_error = false;
        if (!XGetWindowAttributes(ewmh.display, *parent, &attributes) || g_x_error ||
            attributes.override_redirect || attributes.c_class != InputOutput) return std::nullopt;
        const auto bounds = RootGeometry(ewmh.display, *parent);
        if (!bounds || !ValidGeometry(*bounds)) return std::nullopt;
        const auto padding = FramePadding(*client, *bounds);
        if (std::any_of(padding.begin(), padding.end(), [](long long side) {
                return side < 0 || side > 64;
            })) return std::nullopt;
        proof.frames.push_back({*parent, *bounds});
        current = *parent;
    }

    const Atom extents = XInternAtom(ewmh.display, "_NET_FRAME_EXTENTS", False);
    const auto property = ReadProperty32(ewmh.display, window, extents, XA_CARDINAL, 4);
    if (!property) return std::nullopt;
    if (property->present) {
        const auto padding = proof.frames.empty() ? FrameExtents{} :
            FramePadding(*client, proof.frames.back().bounds);
        if (property->values.size() != padding.size() ||
            !std::equal(padding.begin(), padding.end(), property->values.begin(),
                        [](long long side, unsigned long declared) {
                            return static_cast<unsigned long>(side) == declared;
                        })) return std::nullopt;
        proof.extents = padding;
    }
    return proof;
}

std::optional<std::vector<Window>> OwnedWindows(const Ewmh &ewmh, unsigned long pid) {
    if (!PidBelongsToCurrentUser(pid)) return std::nullopt;
    const auto windows = WindowCandidates(ewmh);
    if (!windows) return std::nullopt;
    std::vector<Window> owned;
    for (Window window : *windows) {
        if (!ValidXid(window) || window == ewmh.root) return std::nullopt;
        const auto owner = ReadProperty32(ewmh.display, window, ewmh.pid, XA_CARDINAL, 1);
        if (!owner || (owner->present && owner->values.size() != 1)) return std::nullopt;
        if (owner->values.empty() || owner->values[0] != pid) continue;
        XWindowAttributes attributes{};
        g_x_error = false;
        if (!XGetWindowAttributes(ewmh.display, window, &attributes) || g_x_error) return std::nullopt;
        if (attributes.override_redirect || attributes.c_class != InputOutput ||
            (attributes.map_state != IsViewable && !IcccmState(ewmh, window))) continue;
        // Without EWMH the bounded discovery walk can also see ordinary
        // widget windows. Only root children count as top-level clients.
        if (!ewmh.present) {
            const auto parent = WindowParent(ewmh, window);
            if (!parent) return std::nullopt;
            if (*parent != ewmh.root) continue;
        }
        owned.push_back(window);
    }
    return owned;
}

struct WindowResolution {
    Window client = None;
    std::optional<WindowGeometryProof> proof;
    std::string_view problem = "window_not_found";
};

WindowResolution ResolveWindow(const Ewmh &ewmh, unsigned long pid, const Geometry &observed,
                               bool include_frames) {
    const auto owned = OwnedWindows(ewmh, pid);
    if (!owned || !ValidGeometry(observed)) return {None, {}, "window_inventory_unproven"};
    WindowResolution match;
    for (Window window : *owned) {
        std::optional<WindowGeometryProof> proof;
        if (include_frames) {
            proof = ReadClientGeometry(ewmh, window, pid, observed);
            if (!proof) return {None, {}, "window_mapping_unproven"};
            if (GeometryMatches(proof->client_bounds, observed, 2)) {
                proof->observed_window = window;
            } else {
                for (const auto &frame : proof->frames) {
                    if (GeometryMatches(frame.bounds, observed, 2)) proof->observed_window = frame.id;
                }
            }
            if (!proof->observed_window) continue;
        } else {
            const auto geometry = RootGeometry(ewmh.display, window);
            if (!geometry || !ValidGeometry(*geometry)) return {None, {}, "window_mapping_unproven"};
            if (!GeometryMatches(*geometry, observed, 2)) continue;
        }
        if (match.client != None) return {None, {}, "window_ambiguous"};
        match.client = window;
        match.proof = std::move(proof);
    }
    return match;
}

std::optional<WindowGeometryProof> FindGeometryProof(const Ewmh &ewmh, unsigned long pid,
                                                     const Geometry &observed) {
    return ResolveWindow(ewmh, pid, observed, true).proof;
}

std::string GeometryJson(const Geometry &rect) {
    return "{\"x\":" + std::to_string(rect.x) + ",\"y\":" + std::to_string(rect.y) +
        ",\"width\":" + std::to_string(rect.width) + ",\"height\":" + std::to_string(rect.height) + "}";
}

// This is the private wire representation. aad echoes the object as compact
// JSON in the same field order. Recomputing it checks every field against live
// X11 state without interpreting untrusted values as XIDs or geometry.
std::string GeometryProofJson(const WindowGeometryProof &proof) {
    std::string json = "{\"version\":1,\"expected_pid\":" + std::to_string(proof.pid) +
        ",\"client\":" + std::to_string(proof.client) +
        ",\"observed_bounds\":" + GeometryJson(proof.observed_bounds) +
        ",\"observed_window\":" + std::to_string(proof.observed_window) +
        ",\"client_bounds\":" + GeometryJson(proof.client_bounds) + ",\"frames\":[";
    for (std::size_t i = 0; i < proof.frames.size(); ++i) {
        if (i) json += ',';
        const auto &frame = proof.frames[i];
        json += "{\"xid\":" + std::to_string(frame.id) + ",\"bounds\":" + GeometryJson(frame.bounds) + "}";
    }
    json += ']';
    if (proof.wm) {
        json += ",\"wm_identity\":" + std::to_string(proof.wm->identity) +
            ",\"wm_pid\":" + std::to_string(proof.wm->pid) +
            ",\"resource_mask\":" + std::to_string(proof.wm->mask);
    }
    if (proof.extents) {
        const auto &extents = *proof.extents;
        json += ",\"frame_extents\":[" + std::to_string(extents[0]) + "," + std::to_string(extents[1]) +
            "," + std::to_string(extents[2]) + "," + std::to_string(extents[3]) + "]";
    }
    return json + "}";
}

std::optional<WindowGeometryProof> RecheckGeometryProof(const Ewmh &ewmh, unsigned long pid,
                                                        const WindowBinding &binding) {
    auto proof = FindGeometryProof(ewmh, pid, binding.bounds);
    if (!proof || proof->client != binding.id || GeometryProofJson(*proof) != binding.proof) {
        return std::nullopt;
    }
    return proof;
}
