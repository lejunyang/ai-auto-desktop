#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/keysym.h>
#include <X11/XKBlib.h>
#include <X11/Xproto.h>
#include <X11/extensions/XTest.h>
#include <X11/extensions/record.h>

#include <algorithm>
#include <cctype>
#include <cerrno>
#include <chrono>
#include <climits>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <limits>
#include <optional>
#include <poll.h>
#include <string>
#include <string_view>
#include <sys/stat.h>
#include <unistd.h>
#include <vector>

namespace {

constexpr std::size_t kMaxInputBytes = 4096;
constexpr std::size_t kMaxCodepoints = 1024;
constexpr int kExitInvalid = 64;
constexpr int kExitUnavailable = 69;
constexpr int kExitUnknownEffect = 70;
constexpr int kExitTargetMismatch = 73;
constexpr int kExitUnsupportedText = 74;
constexpr int kExitTimeoutBeforeDispatch = 75;

bool g_x_error = false;

int HandleXError(Display *, XErrorEvent *) {
    g_x_error = true;
    return 0;
}

void EmitError(std::string_view code, std::string_view phase, bool dispatched) {
    std::cout << "{\"ok\":false,\"code\":\"" << code
              << "\",\"phase\":\"" << phase
              << "\",\"dispatch_started\":"
              << (dispatched ? "true" : "false") << "}" << std::endl;
}

bool DeadlineExpired(std::uint64_t deadline_ns) {
    const auto now = std::chrono::steady_clock::now().time_since_epoch();
    const auto now_ns =
        std::chrono::duration_cast<std::chrono::nanoseconds>(now).count();
    return now_ns < 0 || static_cast<std::uint64_t>(now_ns) >= deadline_ns;
}

std::optional<std::uint64_t> ParseUnsigned(std::string_view value) {
    if (value.empty()) {
        return std::nullopt;
    }
    std::uint64_t parsed = 0;
    for (const char item : value) {
        if (item < '0' || item > '9') {
            return std::nullopt;
        }
        const unsigned digit = static_cast<unsigned>(item - '0');
        if (parsed > (std::numeric_limits<std::uint64_t>::max() - digit) / 10) {
            return std::nullopt;
        }
        parsed = parsed * 10 + digit;
    }
    return parsed;
}

bool IsKdeDesktop(std::string_view value) {
    std::size_t start = 0;
    while (start <= value.size()) {
        std::size_t end = value.find_first_of(":;", start);
        if (end == std::string_view::npos) {
            end = value.size();
        }
        std::string token(value.substr(start, end - start));
        for (char &item : token) {
            if (item >= 'a' && item <= 'z') {
                item = static_cast<char>(item - 'a' + 'A');
            }
        }
        if (token == "KDE") {
            return true;
        }
        if (end == value.size()) {
            break;
        }
        start = end + 1;
    }
    return false;
}

bool SessionIsQualified() {
    const char *session_type = std::getenv("XDG_SESSION_TYPE");
    const char *display = std::getenv("DISPLAY");
    const char *desktop = std::getenv("XDG_CURRENT_DESKTOP");
    if (desktop == nullptr || *desktop == '\0') {
        desktop = std::getenv("DESKTOP_SESSION");
    }
    return session_type != nullptr && std::strcmp(session_type, "x11") == 0 &&
           display != nullptr && *display != '\0' && desktop != nullptr &&
           IsKdeDesktop(desktop);
}

std::optional<std::vector<std::uint32_t>> DecodeUtf8(const std::string &input) {
    std::vector<std::uint32_t> result;
    result.reserve(input.size());
    std::size_t index = 0;
    while (index < input.size()) {
        const auto first = static_cast<unsigned char>(input[index]);
        std::uint32_t codepoint = 0;
        std::size_t width = 0;
        if (first <= 0x7f) {
            codepoint = first;
            width = 1;
        } else if (first >= 0xc2 && first <= 0xdf) {
            codepoint = first & 0x1f;
            width = 2;
        } else if (first >= 0xe0 && first <= 0xef) {
            codepoint = first & 0x0f;
            width = 3;
        } else if (first >= 0xf0 && first <= 0xf4) {
            codepoint = first & 0x07;
            width = 4;
        } else {
            return std::nullopt;
        }
        if (index + width > input.size()) {
            return std::nullopt;
        }
        for (std::size_t offset = 1; offset < width; ++offset) {
            const auto continuation =
                static_cast<unsigned char>(input[index + offset]);
            if ((continuation & 0xc0) != 0x80) {
                return std::nullopt;
            }
            codepoint = (codepoint << 6) | (continuation & 0x3f);
        }
        const bool overlong = (width == 2 && codepoint < 0x80) ||
                              (width == 3 && codepoint < 0x800) ||
                              (width == 4 && codepoint < 0x10000);
        const bool surrogate = codepoint >= 0xd800 && codepoint <= 0xdfff;
        const bool noncharacter =
            (codepoint >= 0xfdd0 && codepoint <= 0xfdef) ||
            ((codepoint & 0xffff) == 0xfffe) ||
            ((codepoint & 0xffff) == 0xffff);
        const bool forbidden_control =
            (codepoint < 0x20 && codepoint != '\n') ||
            (codepoint >= 0x7f && codepoint <= 0x9f);
        if (overlong || surrogate || codepoint > 0x10ffff || noncharacter ||
            forbidden_control) {
            return std::nullopt;
        }
        result.push_back(codepoint);
        if (result.size() > kMaxCodepoints) {
            return std::nullopt;
        }
        index += width;
    }
    return result;
}

std::optional<unsigned long> WindowPid(Display *display, Window window,
                                       Atom pid_atom) {
    Window current = window;
    for (int depth = 0; depth < 64 && current != None; ++depth) {
        Atom actual_type = None;
        int actual_format = 0;
        unsigned long count = 0;
        unsigned long bytes_after = 0;
        unsigned char *data = nullptr;
        g_x_error = false;
        const int property_status = XGetWindowProperty(
            display, current, pid_atom, 0, 1, False, XA_CARDINAL, &actual_type,
            &actual_format, &count, &bytes_after, &data);
        XSync(display, False);
        if (!g_x_error && property_status == Success && actual_type == XA_CARDINAL &&
            actual_format == 32 && count == 1 && data != nullptr) {
            const auto pid = *reinterpret_cast<unsigned long *>(data);
            XFree(data);
            return pid;
        }
        if (data != nullptr) {
            XFree(data);
        }
        Window root = None;
        Window parent = None;
        Window *children = nullptr;
        unsigned int child_count = 0;
        g_x_error = false;
        const Status tree_status =
            XQueryTree(display, current, &root, &parent, &children, &child_count);
        XSync(display, False);
        if (children != nullptr) {
            XFree(children);
        }
        if (!tree_status || g_x_error || parent == None || parent == current ||
            current == root) {
            return std::nullopt;
        }
        current = parent;
    }
    return std::nullopt;
}

bool PidBelongsToCurrentUser(unsigned long pid) {
    if (pid == 0 || pid > static_cast<unsigned long>(INT_MAX)) {
        return false;
    }
    struct stat details {};
    const std::string path = "/proc/" + std::to_string(pid);
    return stat(path.c_str(), &details) == 0 && details.st_uid == geteuid();
}

bool FocusBelongsToPid(Display *display, unsigned long expected_pid, Atom pid_atom) {
    Window focus = None;
    int revert_to = 0;
    g_x_error = false;
    XGetInputFocus(display, &focus, &revert_to);
    XSync(display, False);
    if (g_x_error || focus == None || focus == PointerRoot) {
        return false;
    }
    const auto owner = WindowPid(display, focus, pid_atom);
    return owner.has_value() && owner.value() == expected_pid;
}

bool ParseInt(std::string_view value, int *out) {
    if (value.empty() || out == nullptr) {
        return false;
    }
    int sign = 1;
    std::size_t index = 0;
    if (value[0] == '-') {
        sign = -1;
        index = 1;
    }
    if (index >= value.size()) {
        return false;
    }
    long long parsed = 0;
    for (; index < value.size(); ++index) {
        const char item = value[index];
        if (item < '0' || item > '9') {
            return false;
        }
        parsed = parsed * 10 + static_cast<long long>(item - '0');
        if (parsed > std::numeric_limits<int>::max()) {
            return false;
        }
    }
    parsed *= sign;
    if (parsed < std::numeric_limits<int>::min() ||
        parsed > std::numeric_limits<int>::max()) {
        return false;
    }
    *out = static_cast<int>(parsed);
    return true;
}

bool WindowAtPointBelongsToPid(Display *display, int x, int y, unsigned long expected_pid,
                               Atom pid_atom) {
    Window root = DefaultRootWindow(display);
    Window target = None;
    Window parent = root;
    while (true) {
        Window child_return = None;
        int child_x = 0;
        int child_y = 0;
        g_x_error = false;
        const Bool translated = XTranslateCoordinates(
            display, root, parent, x, y, &child_x, &child_y, &child_return);
        XSync(display, False);
        if (!translated || g_x_error) {
            return false;
        }
        if (child_return == None) {
            target = parent;
            break;
        }
        target = child_return;
        parent = child_return;
    }
    if (target == None) {
        return false;
    }
    const auto owner = WindowPid(display, target, pid_atom);
    return owner.has_value() && owner.value() == expected_pid;
}

struct KeyStroke {
    KeyCode keycode = 0;
    bool shift = false;
};

struct DynamicMapping {
    KeyCode keycode = 0;
    KeySym keysym = NoSymbol;
};

struct KeyPlan {
    std::vector<KeyStroke> strokes;
    std::vector<DynamicMapping> dynamic_mappings;
};

std::vector<KeyCode> FindUnusedKeycodes(Display *display, int minimum, int maximum,
                                        int symbols_per_code, KeySym *mapping) {
    XModifierKeymap *modifiers = XGetModifierMapping(display);
    if (modifiers == nullptr) {
        return {};
    }
    std::vector<bool> is_modifier(static_cast<std::size_t>(maximum + 1), false);
    const int modifier_slots = 8 * modifiers->max_keypermod;
    for (int index = 0; index < modifier_slots; ++index) {
        const KeyCode code = modifiers->modifiermap[index];
        if (code <= maximum) {
            is_modifier[code] = true;
        }
    }
    XFreeModifiermap(modifiers);
    std::vector<KeyCode> result;
    for (int code = maximum; code >= minimum; --code) {
        if (is_modifier[static_cast<std::size_t>(code)]) {
            continue;
        }
        bool unused = true;
        for (int level = 0; level < symbols_per_code; ++level) {
            if (mapping[(code - minimum) * symbols_per_code + level] != NoSymbol) {
                unused = false;
                break;
            }
        }
        if (unused) {
            result.push_back(static_cast<KeyCode>(code));
        }
    }
    return result;
}

std::optional<KeyPlan> BuildKeyPlan(
    const std::vector<std::uint32_t> &codepoints, int minimum,
    int maximum, int symbols_per_code, KeySym *mapping,
    const std::vector<KeyCode> &spares) {
    KeyPlan plan;
    plan.strokes.reserve(codepoints.size());
    for (const std::uint32_t codepoint : codepoints) {
        const KeySym wanted =
            codepoint == '\n'
                ? XK_Return
                : (codepoint <= 0xff ? static_cast<KeySym>(codepoint)
                                     : static_cast<KeySym>(0x01000000U | codepoint));
        bool found = false;
        for (int code = minimum; code <= maximum && !found; ++code) {
            for (int level = 0; level < symbols_per_code && level < 2; ++level) {
                if (mapping[(code - minimum) * symbols_per_code + level] == wanted) {
                    plan.strokes.push_back({static_cast<KeyCode>(code), level == 1});
                    found = true;
                    break;
                }
            }
        }
        if (!found) {
            const auto existing = std::find_if(
                plan.dynamic_mappings.begin(),
                plan.dynamic_mappings.end(),
                [wanted](const DynamicMapping &item) { return item.keysym == wanted; });
            if (existing != plan.dynamic_mappings.end()) {
                plan.strokes.push_back(KeyStroke{existing->keycode, false});
                continue;
            }
            if (wanted == NoSymbol || plan.dynamic_mappings.size() >= spares.size()) {
                return std::nullopt;
            }
            const KeyCode keycode = spares[plan.dynamic_mappings.size()];
            plan.dynamic_mappings.push_back({keycode, wanted});
            plan.strokes.push_back({keycode, false});
        }
    }
    return plan;
}

// Gestures, drags and key chords. Each runs after the shared preflight in
// main: the session qualifies, focus belongs to the expected process and every
// point lands on one of its windows.
struct GestureRequest {
    std::string mode;  // click, wheel or hover
    int x = 0;
    int y = 0;
    int to_x = 0;
    int to_y = 0;
    unsigned button = 1;
    unsigned count = 1;
    unsigned notches = 3;
    unsigned hover_ms = 500;
    std::vector<std::string> keys;
};

// `--name value` pairs. Every name must be expected and appear once; the
// caller checks which ones are required.
std::optional<std::vector<std::pair<std::string, std::string>>> ParsePairs(
    int argc, char **argv, const std::vector<std::string_view> &allowed) {
    std::vector<std::pair<std::string, std::string>> pairs;
    for (int index = 2; index < argc; index += 2) {
        if (index + 1 >= argc) {
            return std::nullopt;
        }
        const std::string_view name(argv[index]);
        if (name.size() < 3 || name.substr(0, 2) != "--" ||
            std::find(allowed.begin(), allowed.end(), name.substr(2)) == allowed.end()) {
            return std::nullopt;
        }
        for (const auto &seen : pairs) {
            if (seen.first == name.substr(2)) {
                return std::nullopt;
            }
        }
        pairs.emplace_back(std::string(name.substr(2)), std::string(argv[index + 1]));
    }
    return pairs;
}

std::optional<std::string> Pair(const std::vector<std::pair<std::string, std::string>> &pairs,
                                std::string_view name) {
    for (const auto &pair : pairs) {
        if (pair.first == name) {
            return pair.second;
        }
    }
    return std::nullopt;
}

std::optional<unsigned> BoundedUnsigned(const std::optional<std::string> &value, unsigned low,
                                        unsigned high) {
    if (!value.has_value()) {
        return std::nullopt;
    }
    const auto parsed = ParseUnsigned(value.value());
    if (!parsed.has_value() || parsed.value() < low || parsed.value() > high) {
        return std::nullopt;
    }
    return static_cast<unsigned>(parsed.value());
}

// A key name as the control protocol spells it, to an X keysym.
KeySym KeyNamed(const std::string &raw) {
    std::string name = raw;
    for (char &item : name) {
        item = static_cast<char>(std::tolower(static_cast<unsigned char>(item)));
    }
    static const std::vector<std::pair<std::string_view, KeySym>> named = {
        {"ctrl", XK_Control_L},  {"control", XK_Control_L}, {"shift", XK_Shift_L},
        {"alt", XK_Alt_L},       {"win", XK_Super_L},       {"super", XK_Super_L},
        {"meta", XK_Super_L},    {"enter", XK_Return},      {"return", XK_Return},
        {"tab", XK_Tab},         {"escape", XK_Escape},     {"esc", XK_Escape},
        {"backspace", XK_BackSpace}, {"delete", XK_Delete}, {"insert", XK_Insert},
        {"home", XK_Home},       {"end", XK_End},           {"pageup", XK_Prior},
        {"pagedown", XK_Next},   {"left", XK_Left},         {"right", XK_Right},
        {"up", XK_Up},           {"down", XK_Down},         {"space", XK_space},
    };
    for (const auto &entry : named) {
        if (entry.first == name) {
            return entry.second;
        }
    }
    if (name.size() >= 2 && name.size() <= 3 && name[0] == 'f') {
        const auto number = ParseUnsigned(std::string_view(name).substr(1));
        if (number.has_value() && number.value() >= 1 && number.value() <= 24) {
            return XK_F1 + static_cast<KeySym>(number.value() - 1);
        }
    }
    if (name.size() == 1 && std::isalnum(static_cast<unsigned char>(name[0]))) {
        return XStringToKeysym(name.c_str());
    }
    return NoSymbol;
}

bool IsModifier(KeySym symbol) {
    return symbol == XK_Control_L || symbol == XK_Shift_L || symbol == XK_Alt_L ||
           symbol == XK_Super_L;
}

void SleepUntilOrDeadline(unsigned milliseconds, std::uint64_t deadline_ns) {
    const auto until = std::chrono::steady_clock::now() + std::chrono::milliseconds(milliseconds);
    while (std::chrono::steady_clock::now() < until && !DeadlineExpired(deadline_ns)) {
        usleep(5000);
    }
}

int RunGesture(Display *display, std::string_view command, const GestureRequest &request,
               std::uint64_t expected_pid, std::uint64_t deadline_ns, Atom pid_atom) {
    std::vector<KeyCode> chord;
    if (command == "key-chord") {
        // Modifiers first, then exactly one main key, as the protocol requires.
        bool seen_main = false;
        for (std::size_t index = 0; index < request.keys.size(); ++index) {
            const KeySym symbol = KeyNamed(request.keys[index]);
            const KeyCode code = symbol == NoSymbol ? 0 : XKeysymToKeycode(display, symbol);
            const bool last = index + 1 == request.keys.size();
            if (code == 0 || IsModifier(symbol) == last || seen_main) {
                XCloseDisplay(display);
                EmitError("unsupported_key", "key_preflight", false);
                return kExitInvalid;
            }
            seen_main = last;
            chord.push_back(code);
        }
    } else {
        std::vector<std::pair<int, int>> points = {{request.x, request.y}};
        if (command == "drag") {
            points.emplace_back(request.to_x, request.to_y);
        }
        for (const auto &point : points) {
            if (!WindowAtPointBelongsToPid(display, point.first, point.second, expected_pid,
                                           pid_atom)) {
                XCloseDisplay(display);
                EmitError("target_point_mismatch", "target_preflight", false);
                return kExitTargetMismatch;
            }
        }
    }
    bool dispatch_started = false;
    auto finish_failure = [&](std::string_view code, std::string_view phase, int pre_dispatch_exit) {
        // Never leave a button or a modifier held down.
        if (dispatch_started) {
            if (command == "drag") {
                XTestFakeButtonEvent(display, request.button, False, 1);
            }
            for (auto code_it = chord.rbegin(); code_it != chord.rend(); ++code_it) {
                XTestFakeKeyEvent(display, *code_it, False, 1);
            }
        }
        XSync(display, False);
        XCloseDisplay(display);
        EmitError(code, phase, dispatch_started);
        return dispatch_started ? kExitUnknownEffect : pre_dispatch_exit;
    };
    if (DeadlineExpired(deadline_ns)) {
        return finish_failure("deadline_exceeded", "pre_dispatch", kExitTimeoutBeforeDispatch);
    }
    if (!FocusBelongsToPid(display, expected_pid, pid_atom)) {
        return finish_failure("focus_owner_changed", "target_preflight", kExitTargetMismatch);
    }
    std::cout << "{\"event\":\"dispatch_started\"}" << std::endl;
    dispatch_started = true;
    g_x_error = false;
    bool submitted = true;
    unsigned long event_count = 0;
    auto submit = [&](bool ok) {
        submitted = ok && submitted;
        ++event_count;
    };
    if (command == "key-chord") {
        for (const KeyCode code : chord) {
            submit(XTestFakeKeyEvent(display, code, True, 1) != 0);
        }
        for (auto code_it = chord.rbegin(); code_it != chord.rend(); ++code_it) {
            submit(XTestFakeKeyEvent(display, *code_it, False, 1) != 0);
        }
    } else {
        submit(XTestFakeMotionEvent(display, -1, request.x, request.y, 1) != 0);
        if (command == "drag") {
            XSync(display, False);
            submit(XTestFakeButtonEvent(display, request.button, True, 1) != 0);
            XSync(display, False);
            // Glide in steps: many toolkits only start a drag after the
            // pointer has moved a few pixels with the button held.
            constexpr int kSteps = 12;
            for (int step = 1; step <= kSteps; ++step) {
                const int x = request.x + (request.to_x - request.x) * step / kSteps;
                const int y = request.y + (request.to_y - request.y) * step / kSteps;
                submit(XTestFakeMotionEvent(display, -1, x, y, 1) != 0);
                XSync(display, False);
                SleepUntilOrDeadline(15, deadline_ns);
            }
            submit(XTestFakeButtonEvent(display, request.button, False, 1) != 0);
        } else if (request.mode == "click") {
            for (unsigned click = 0; click < request.count; ++click) {
                submit(XTestFakeButtonEvent(display, request.button, True, 1) != 0);
                submit(XTestFakeButtonEvent(display, request.button, False, 1) != 0);
            }
        } else if (request.mode == "wheel") {
            for (unsigned notch = 0; notch < request.notches; ++notch) {
                submit(XTestFakeButtonEvent(display, request.button, True, 1) != 0);
                submit(XTestFakeButtonEvent(display, request.button, False, 1) != 0);
            }
        } else {
            XSync(display, False);
            SleepUntilOrDeadline(request.hover_ms, deadline_ns);
        }
    }
    XSync(display, False);
    if (!submitted || g_x_error) {
        return finish_failure("x11_error", "input_dispatch", kExitUnknownEffect);
    }
    XCloseDisplay(display);
    std::cout << "{\"ok\":true,\"dispatch_started\":true,\"events\":" << event_count << "}"
              << std::endl;
    return 0;
}

// Parse a gesture command's arguments; nullopt is a usage error.
std::optional<GestureRequest> ParseGesture(std::string_view command, int argc, char **argv,
                                           std::optional<std::uint64_t> *expected_pid,
                                           std::optional<std::uint64_t> *deadline_ns) {
    std::vector<std::string_view> allowed = {"expected-pid", "deadline-monotonic-ns"};
    if (command == "pointer-gesture") {
        allowed.insert(allowed.end(), {"mode", "x", "y", "button", "count", "notches", "hover-ms"});
    } else if (command == "drag") {
        allowed.insert(allowed.end(), {"x", "y", "to-x", "to-y", "button"});
    } else {
        allowed.push_back("keys");
    }
    const auto pairs = ParsePairs(argc, argv, allowed);
    if (!pairs.has_value()) {
        return std::nullopt;
    }
    const auto pid = Pair(*pairs, "expected-pid");
    const auto deadline = Pair(*pairs, "deadline-monotonic-ns");
    if (!pid.has_value() || !deadline.has_value()) {
        return std::nullopt;
    }
    *expected_pid = ParseUnsigned(*pid);
    *deadline_ns = ParseUnsigned(*deadline);
    GestureRequest request;
    auto coordinate = [&](std::string_view name, int *out) {
        const auto value = Pair(*pairs, name);
        return value.has_value() && ParseInt(*value, out);
    };
    if (command == "key-chord") {
        const auto keys = Pair(*pairs, "keys");
        if (!keys.has_value() || keys->empty() || keys->size() > 256) {
            return std::nullopt;
        }
        std::size_t start = 0;
        while (start <= keys->size()) {
            const std::size_t end = std::min(keys->find(',', start), keys->size());
            request.keys.push_back(keys->substr(start, end - start));
            start = end + 1;
        }
        if (request.keys.empty() || request.keys.size() > 6) {
            return std::nullopt;
        }
        return request;
    }
    if (!coordinate("x", &request.x) || !coordinate("y", &request.y)) {
        return std::nullopt;
    }
    const auto button = Pair(*pairs, "button");
    if (command == "drag") {
        const auto parsed = button.has_value() ? BoundedUnsigned(button, 1, 3) : 1u;
        if (!parsed.has_value() || !coordinate("to-x", &request.to_x) ||
            !coordinate("to-y", &request.to_y)) {
            return std::nullopt;
        }
        request.button = parsed.value();
        return request;
    }
    const auto mode = Pair(*pairs, "mode");
    if (!mode.has_value()) {
        return std::nullopt;
    }
    request.mode = *mode;
    if (request.mode == "click") {
        const auto parsed = BoundedUnsigned(button, 1, 3);
        const auto count = BoundedUnsigned(Pair(*pairs, "count"), 1, 2);
        if (!parsed.has_value() || !count.has_value() || Pair(*pairs, "notches").has_value() ||
            Pair(*pairs, "hover-ms").has_value()) {
            return std::nullopt;
        }
        request.button = parsed.value();
        request.count = count.value();
    } else if (request.mode == "wheel") {
        // X11 reports the wheel as buttons 4 (up) to 7 (right).
        const auto parsed = BoundedUnsigned(button, 4, 7);
        const auto notches = BoundedUnsigned(Pair(*pairs, "notches"), 1, 30);
        if (!parsed.has_value() || !notches.has_value() || Pair(*pairs, "count").has_value() ||
            Pair(*pairs, "hover-ms").has_value()) {
            return std::nullopt;
        }
        request.button = parsed.value();
        request.notches = notches.value();
    } else if (request.mode == "hover") {
        const auto dwell = BoundedUnsigned(Pair(*pairs, "hover-ms"), 1, 5000);
        if (!dwell.has_value() || button.has_value() || Pair(*pairs, "count").has_value() ||
            Pair(*pairs, "notches").has_value()) {
            return std::nullopt;
        }
        request.hover_ms = dwell.value();
    } else {
        return std::nullopt;
    }
    return request;
}

// Watching: report the person's clicks, wheel turns and key presses on the
// expected process, one JSON line each, until stdin closes or the deadline.
// Read-only: nothing is injected, and input elsewhere is not reported.
struct RecordState {
    Display *control = nullptr;
    unsigned long pid = 0;
    Atom pid_atom = None;
};
RecordState g_record;

std::uint64_t NowMs() {
    return static_cast<std::uint64_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
                                          std::chrono::steady_clock::now().time_since_epoch())
                                          .count());
}

bool IsModifierKeysym(KeySym symbol) {
    return symbol == XK_Shift_L || symbol == XK_Shift_R || symbol == XK_Control_L ||
           symbol == XK_Control_R || symbol == XK_Alt_L || symbol == XK_Alt_R ||
           symbol == XK_Super_L || symbol == XK_Super_R || symbol == XK_Meta_L ||
           symbol == XK_Meta_R || symbol == XK_ISO_Level3_Shift || symbol == XK_Caps_Lock;
}

void OnRecorded(XPointer, XRecordInterceptData *data) {
    if (data->category == XRecordFromServer && data->data_len * 4 >= sizeof(xEvent)) {
        const auto *event = reinterpret_cast<const xEvent *>(data->data);
        const int type = event->u.u.type & 0x7f;
        const unsigned detail = event->u.u.detail;
        const int x = event->u.keyButtonPointer.rootX;
        const int y = event->u.keyButtonPointer.rootY;
        const unsigned state = event->u.keyButtonPointer.state;
        if (type == ButtonPress && detail >= 1 && detail <= 7 &&
            WindowAtPointBelongsToPid(g_record.control, x, y, g_record.pid, g_record.pid_atom)) {
            std::cout << "{\"event\":\"button\",\"button\":" << detail << ",\"x\":" << x
                      << ",\"y\":" << y << ",\"at_ms\":" << NowMs() << "}" << std::endl;
        } else if (type == KeyPress &&
                   FocusBelongsToPid(g_record.control, g_record.pid, g_record.pid_atom)) {
            const KeySym symbol =
                XkbKeycodeToKeysym(g_record.control, static_cast<KeyCode>(detail), 0, 0);
            const char *name = symbol == NoSymbol ? nullptr : XKeysymToString(symbol);
            bool plain = name != nullptr && name[0] != '\0' && std::strlen(name) <= 32;
            for (const char *c = name; plain && *c; ++c) {
                plain = std::isalnum(static_cast<unsigned char>(*c)) || *c == '_';
            }
            if (plain && !IsModifierKeysym(symbol)) {
                std::cout << "{\"event\":\"key\",\"key\":\"" << name
                          << "\",\"ctrl\":" << ((state & ControlMask) ? "true" : "false")
                          << ",\"shift\":" << ((state & ShiftMask) ? "true" : "false")
                          << ",\"alt\":" << ((state & Mod1Mask) ? "true" : "false")
                          << ",\"super\":" << ((state & Mod4Mask) ? "true" : "false")
                          << ",\"at_ms\":" << NowMs() << "}" << std::endl;
            }
        }
    }
    XRecordFreeData(data);
}

int RunRecordInput(int argc, char **argv) {
    const auto pairs = ParsePairs(argc, argv, {"expected-pid", "deadline-monotonic-ns"});
    if (!pairs.has_value()) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const auto pid = ParseUnsigned(Pair(*pairs, "expected-pid").value_or(""));
    const auto deadline = ParseUnsigned(Pair(*pairs, "deadline-monotonic-ns").value_or(""));
    if (!pid.has_value() || pid.value() == 0 ||
        pid.value() > static_cast<std::uint64_t>(INT_MAX) || !deadline.has_value()) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    if (!SessionIsQualified()) {
        EmitError("unsupported_session", "session_preflight", false);
        return kExitUnavailable;
    }
    if (!PidBelongsToCurrentUser(static_cast<unsigned long>(pid.value()))) {
        EmitError("untrusted_process", "target_preflight", false);
        return kExitTargetMismatch;
    }
    XSetErrorHandler(HandleXError);
    Display *control = XOpenDisplay(nullptr);
    Display *data = XOpenDisplay(nullptr);
    if (control == nullptr || data == nullptr) {
        if (control != nullptr) XCloseDisplay(control);
        if (data != nullptr) XCloseDisplay(data);
        EmitError("display_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    int major = 0;
    int minor = 0;
    const Atom pid_atom = XInternAtom(control, "_NET_WM_PID", True);
    if (!XRecordQueryVersion(control, &major, &minor) || pid_atom == None) {
        XCloseDisplay(control);
        XCloseDisplay(data);
        EmitError("xrecord_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    g_record = {control, static_cast<unsigned long>(pid.value()), pid_atom};
    XRecordRange *range = XRecordAllocRange();
    if (range == nullptr) {
        XCloseDisplay(control);
        XCloseDisplay(data);
        EmitError("xrecord_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    range->device_events.first = KeyPress;
    range->device_events.last = ButtonPress;
    XRecordClientSpec clients = XRecordAllClients;
    const XRecordContext context = XRecordCreateContext(control, 0, &clients, 1, &range, 1);
    XSync(control, False);
    if (context == 0 || !XRecordEnableContextAsync(data, context, OnRecorded, nullptr)) {
        XFree(range);
        XCloseDisplay(control);
        XCloseDisplay(data);
        EmitError("xrecord_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    std::cout << "{\"event\":\"ready\"}" << std::endl;
    while (!DeadlineExpired(deadline.value())) {
        pollfd fds[2] = {{ConnectionNumber(data), POLLIN, 0}, {0, POLLIN, 0}};
        poll(fds, 2, 100);
        if (fds[1].revents & (POLLIN | POLLHUP | POLLERR)) {
            char ignored = 0;
            if (read(0, &ignored, 1) <= 0) break;
        }
        XRecordProcessReplies(data);
    }
    XRecordDisableContext(control, context);
    XRecordFreeContext(control, context);
    XSync(control, False);
    XFree(range);
    XCloseDisplay(data);
    XCloseDisplay(control);
    std::cout << "{\"event\":\"stopped\"}" << std::endl;
    return 0;
}

}  // namespace

int main(int argc, char **argv) {
    if (argc < 2) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const std::string_view command(argv[1]);
    if (command == "record-input") {
        return RunRecordInput(argc, argv);
    }
    std::optional<std::uint64_t> expected_pid;
    std::optional<std::uint64_t> deadline_ns;
    std::optional<int> click_x;
    std::optional<int> click_y;
    std::optional<GestureRequest> gesture;
    if (command == "pointer-gesture" || command == "drag" || command == "key-chord") {
        gesture = ParseGesture(command, argc, argv, &expected_pid, &deadline_ns);
        if (!gesture.has_value()) {
            EmitError("invalid_arguments", "arguments", false);
            return kExitInvalid;
        }
    } else if (command == "type-text") {
        if (argc != 6 || std::string_view(argv[2]) != "--expected-pid" ||
            std::string_view(argv[4]) != "--deadline-monotonic-ns") {
            EmitError("invalid_arguments", "arguments", false);
            return kExitInvalid;
        }
        expected_pid = ParseUnsigned(argv[3]);
        deadline_ns = ParseUnsigned(argv[5]);
    } else if (command == "pointer-click") {
        if (argc != 10 || std::string_view(argv[2]) != "--expected-pid" ||
            std::string_view(argv[4]) != "--x" || std::string_view(argv[6]) != "--y" ||
            std::string_view(argv[8]) != "--deadline-monotonic-ns") {
            EmitError("invalid_arguments", "arguments", false);
            return kExitInvalid;
        }
        expected_pid = ParseUnsigned(argv[3]);
        deadline_ns = ParseUnsigned(argv[9]);
        int parsed_x = 0;
        int parsed_y = 0;
        if (!ParseInt(argv[5], &parsed_x) || !ParseInt(argv[7], &parsed_y)) {
            EmitError("invalid_arguments", "arguments", false);
            return kExitInvalid;
        }
        click_x = parsed_x;
        click_y = parsed_y;
    } else {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    if (!expected_pid.has_value() || expected_pid.value() == 0 ||
        expected_pid.value() > static_cast<std::uint64_t>(INT_MAX) ||
        !deadline_ns.has_value()) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    if (!SessionIsQualified()) {
        EmitError("unsupported_session", "session_preflight", false);
        return kExitUnavailable;
    }
    if (!PidBelongsToCurrentUser(static_cast<unsigned long>(expected_pid.value()))) {
        EmitError("untrusted_process", "target_preflight", false);
        return kExitTargetMismatch;
    }

    std::optional<std::vector<std::uint32_t>> codepoints;
    if (command == "type-text") {
        std::string input;
        input.reserve(kMaxInputBytes);
        char buffer[1024];
        while (std::cin.good()) {
            std::cin.read(buffer, sizeof(buffer));
            const std::streamsize count = std::cin.gcount();
            if (count > 0) {
                input.append(buffer, static_cast<std::size_t>(count));
                if (input.size() > kMaxInputBytes) {
                    EmitError("input_too_large", "text_preflight", false);
                    return kExitInvalid;
                }
            }
        }
        codepoints = DecodeUtf8(input);
        if (!codepoints.has_value() || codepoints->empty()) {
            EmitError("unsupported_text", "text_preflight", false);
            return kExitUnsupportedText;
        }
    }
    if (DeadlineExpired(deadline_ns.value())) {
        EmitError("deadline_exceeded", "pre_dispatch", false);
        return kExitTimeoutBeforeDispatch;
    }

    XSetErrorHandler(HandleXError);
    Display *display = XOpenDisplay(nullptr);
    if (display == nullptr) {
        EmitError("display_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    int event_base = 0;
    int error_base = 0;
    int major = 0;
    int minor = 0;
    if (!XTestQueryExtension(display, &event_base, &error_base, &major, &minor)) {
        XCloseDisplay(display);
        EmitError("xtest_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", True);
    if (pid_atom == None) {
        XCloseDisplay(display);
        EmitError("pid_property_unavailable", "target_preflight", false);
        return kExitTargetMismatch;
    }
    while (!FocusBelongsToPid(display, expected_pid.value(), pid_atom)) {
        if (DeadlineExpired(deadline_ns.value())) {
            XCloseDisplay(display);
            EmitError("focus_owner_mismatch", "target_preflight", false);
            return kExitTargetMismatch;
        }
        usleep(5000);
    }
    if (gesture.has_value()) {
        return RunGesture(display, command, gesture.value(), expected_pid.value(),
                          deadline_ns.value(), pid_atom);
    }
    if (command == "pointer-click") {
        if (!click_x.has_value() || !click_y.has_value() ||
            !WindowAtPointBelongsToPid(display, click_x.value(), click_y.value(),
                                       expected_pid.value(), pid_atom)) {
            XCloseDisplay(display);
            EmitError("target_point_mismatch", "target_preflight", false);
            return kExitTargetMismatch;
        }
        bool dispatch_started = false;
        unsigned long event_count = 0;
        auto finish_failure = [&](std::string_view code, std::string_view phase,
                                  int pre_dispatch_exit) {
            XSync(display, False);
            XCloseDisplay(display);
            EmitError(code, phase, dispatch_started);
            return dispatch_started ? kExitUnknownEffect : pre_dispatch_exit;
        };
        if (DeadlineExpired(deadline_ns.value())) {
            return finish_failure("deadline_exceeded", "pre_dispatch",
                                  kExitTimeoutBeforeDispatch);
        }
        if (!FocusBelongsToPid(display, expected_pid.value(), pid_atom)) {
            return finish_failure("focus_owner_changed", "target_preflight",
                                  kExitTargetMismatch);
        }
        std::cout << "{\"event\":\"dispatch_started\"}" << std::endl;
        dispatch_started = true;
        g_x_error = false;
        bool submitted = XTestFakeMotionEvent(display, -1, click_x.value(), click_y.value(), 1) != 0;
        submitted = XTestFakeButtonEvent(display, 1, True, 1) != 0 && submitted;
        submitted = XTestFakeButtonEvent(display, 1, False, 1) != 0 && submitted;
        event_count += 3;
        XSync(display, False);
        if (!submitted || g_x_error) {
            return finish_failure("x11_error", "pointer_dispatch", kExitUnknownEffect);
        }
        const bool final_error = g_x_error;
        XCloseDisplay(display);
        if (final_error) {
            EmitError("x11_error", "post_dispatch", dispatch_started);
            return dispatch_started ? kExitUnknownEffect : kExitUnavailable;
        }
        std::cout << "{\"ok\":true,\"dispatch_started\":"
                  << (dispatch_started ? "true" : "false")
                  << ",\"events\":" << event_count << "}" << std::endl;
        return 0;
    }

    int minimum = 0;
    int maximum = 0;
    XDisplayKeycodes(display, &minimum, &maximum);
    int symbols_per_code = 0;
    KeySym *mapping =
        XGetKeyboardMapping(display, static_cast<KeyCode>(minimum),
                            maximum - minimum + 1, &symbols_per_code);
    if (mapping == nullptr || symbols_per_code < 1) {
        if (mapping != nullptr) {
            XFree(mapping);
        }
        XCloseDisplay(display);
        EmitError("keyboard_map_unavailable", "text_preflight", false);
        return kExitUnavailable;
    }
    const auto spares =
        FindUnusedKeycodes(display, minimum, maximum, symbols_per_code, mapping);
    const auto plan = BuildKeyPlan(codepoints.value(), minimum, maximum,
                                   symbols_per_code, mapping, spares);
    if (!plan.has_value()) {
        XFree(mapping);
        XCloseDisplay(display);
        EmitError("unsupported_keyboard_mapping", "text_preflight", false);
        return kExitUnsupportedText;
    }
    const KeyCode shift = XKeysymToKeycode(display, XK_Shift_L);
    bool requires_shift = false;
    for (const auto &stroke : plan->strokes) {
        requires_shift = requires_shift || stroke.shift;
    }
    if (requires_shift && shift == 0) {
        XFree(mapping);
        XCloseDisplay(display);
        EmitError("shift_unavailable", "text_preflight", false);
        return kExitUnsupportedText;
    }

    std::vector<std::vector<KeySym>> original_mappings;
    original_mappings.reserve(plan->dynamic_mappings.size());
    for (const auto &dynamic : plan->dynamic_mappings) {
        const int base = (static_cast<int>(dynamic.keycode) - minimum) * symbols_per_code;
        original_mappings.emplace_back(
            mapping + base, mapping + base + symbols_per_code);
    }
    XFree(mapping);

    bool dispatch_started = false;
    bool dynamic_mapping_changed = false;
    unsigned long event_count = 0;
    auto restore_mapping = [&]() {
        if (dynamic_mapping_changed) {
            g_x_error = false;
            for (std::size_t index = 0; index < plan->dynamic_mappings.size(); ++index) {
                XChangeKeyboardMapping(
                    display, static_cast<int>(plan->dynamic_mappings[index].keycode),
                    symbols_per_code, original_mappings[index].data(), 1);
            }
            XSync(display, False);
            dynamic_mapping_changed = false;
        }
    };
    auto finish_failure = [&](std::string_view code, std::string_view phase,
                              int pre_dispatch_exit) {
        restore_mapping();
        XSync(display, False);
        XCloseDisplay(display);
        EmitError(code, phase, dispatch_started);
        return dispatch_started ? kExitUnknownEffect : pre_dispatch_exit;
    };
    if (!plan->dynamic_mappings.empty()) {
        g_x_error = false;
        for (const auto &dynamic : plan->dynamic_mappings) {
            std::vector<KeySym> replacement(
                static_cast<std::size_t>(symbols_per_code), NoSymbol);
            replacement[0] = dynamic.keysym;
            XChangeKeyboardMapping(display, static_cast<int>(dynamic.keycode),
                                   symbols_per_code, replacement.data(), 1);
        }
        dynamic_mapping_changed = true;
        XSync(display, False);
        if (g_x_error) {
            return finish_failure("keyboard_map_failed", "text_preflight",
                                  kExitUnsupportedText);
        }
        if (DeadlineExpired(deadline_ns.value())) {
            return finish_failure("deadline_exceeded", "pre_dispatch",
                                  kExitTimeoutBeforeDispatch);
        }
        // Let GTK/Qt consume the MappingNotify events before the first key.
        usleep(50'000);
    }

    for (const auto &stroke : plan->strokes) {
        if (DeadlineExpired(deadline_ns.value())) {
            return finish_failure("deadline_exceeded", "keyboard_dispatch",
                                  kExitTimeoutBeforeDispatch);
        }
        if (!FocusBelongsToPid(display, expected_pid.value(), pid_atom)) {
            return finish_failure("focus_owner_changed", "keyboard_dispatch",
                                  kExitTargetMismatch);
        }
        if (!dispatch_started) {
            std::cout << "{\"event\":\"dispatch_started\"}" << std::endl;
            dispatch_started = true;
        }
        g_x_error = false;
        bool submitted = true;
        if (stroke.shift) {
            submitted = XTestFakeKeyEvent(display, shift, True, 1) != 0;
            ++event_count;
        }
        submitted =
            XTestFakeKeyEvent(display, stroke.keycode, True, 1) != 0 && submitted;
        submitted =
            XTestFakeKeyEvent(display, stroke.keycode, False, 1) != 0 && submitted;
        event_count += 2;
        if (stroke.shift) {
            submitted =
                XTestFakeKeyEvent(display, shift, False, 1) != 0 && submitted;
            ++event_count;
        }
        XSync(display, False);
        if (!submitted || g_x_error) {
            return finish_failure("x11_error", "keyboard_dispatch",
                                  kExitUnknownEffect);
        }
    }
    restore_mapping();
    XSync(display, False);
    const bool final_error = g_x_error;
    XCloseDisplay(display);
    if (final_error) {
        EmitError("x11_error", "post_dispatch", dispatch_started);
        return dispatch_started ? kExitUnknownEffect : kExitUnavailable;
    }
    std::cout << "{\"ok\":true,\"dispatch_started\":"
              << (dispatch_started ? "true" : "false")
              << ",\"events\":" << event_count
              << ",\"codepoints\":" << codepoints->size() << "}"
              << std::endl;
    return 0;
}
