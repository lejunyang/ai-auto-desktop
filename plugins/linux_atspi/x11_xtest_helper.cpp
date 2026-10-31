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
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
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

// ---------------------------------------------------------------------------
// Windows, the clipboard and global keys.
//
// window-info / window-action find the one top-level client window of the
// expected process whose root geometry is the observed AT-SPI frame, and use
// EWMH/ICCCM requests to the window manager; an action waits (bounded) until
// the window manager reports the state it asked for. clipboard-read reads the
// CLIPBOARD selection's text. paste owns CLIPBOARD with the given text, serves
// it only to the expected process after Ctrl+V, then hands the previous
// contents to a short-lived restorer. global-key presses a chord on whatever
// has focus, anchored on nothing.
// ---------------------------------------------------------------------------

constexpr int kExitTooLarge = 76;
constexpr int kExitProtected = 77;
constexpr std::size_t kMaxPasteBytes = 16 * 1024;
constexpr std::size_t kMaxSavedClipboardBytes = 32 * 1024 * 1024;

std::string JsonString(const std::string &raw) {
    // Invalid UTF-8 becomes U+FFFD so the reader always gets valid JSON.
    std::string out = "\"";
    std::size_t index = 0;
    while (index < raw.size()) {
        const auto first = static_cast<unsigned char>(raw[index]);
        std::size_t width = first < 0x80 ? 1
                            : (first >= 0xc2 && first <= 0xdf) ? 2
                            : (first >= 0xe0 && first <= 0xef) ? 3
                            : (first >= 0xf0 && first <= 0xf4) ? 4
                                                               : 0;
        bool valid = width != 0 && index + width <= raw.size();
        for (std::size_t offset = 1; valid && offset < width; ++offset) {
            valid = (static_cast<unsigned char>(raw[index + offset]) & 0xc0) == 0x80;
        }
        if (!valid) {
            out += "\\ufffd";
            ++index;
            continue;
        }
        if (width == 1) {
            const char item = raw[index];
            if (item == '"' || item == '\\') {
                out += '\\';
                out += item;
            } else if (first < 0x20 || first == 0x7f) {
                static const char *hex = "0123456789abcdef";
                out += "\\u00";
                out += hex[first >> 4];
                out += hex[first & 0xf];
            } else {
                out += item;
            }
        } else {
            out.append(raw, index, width);
        }
        index += width;
    }
    out += "\"";
    return out;
}

std::optional<std::string> ReadStdin(std::size_t limit) {
    std::string input;
    char buffer[1024];
    while (std::cin.good()) {
        std::cin.read(buffer, sizeof(buffer));
        const std::streamsize count = std::cin.gcount();
        if (count > 0) {
            input.append(buffer, static_cast<std::size_t>(count));
            if (input.size() > limit) {
                return std::nullopt;
            }
        }
    }
    return input;
}

std::vector<Atom> AtomList(Display *display, Window window, Atom property) {
    std::vector<Atom> result;
    Atom actual_type = None;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long bytes_after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const int status = XGetWindowProperty(display, window, property, 0, 4096, False, XA_ATOM,
                                          &actual_type, &actual_format, &count, &bytes_after,
                                          &data);
    XSync(display, False);
    if (!g_x_error && status == Success && actual_type == XA_ATOM && actual_format == 32 &&
        data != nullptr) {
        const auto *atoms = reinterpret_cast<Atom *>(data);
        result.assign(atoms, atoms + count);
    }
    if (data != nullptr) {
        XFree(data);
    }
    return result;
}

std::optional<Window> WindowProperty(Display *display, Window window, Atom property) {
    Atom actual_type = None;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long bytes_after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const int status = XGetWindowProperty(display, window, property, 0, 1, False, XA_WINDOW,
                                          &actual_type, &actual_format, &count, &bytes_after,
                                          &data);
    XSync(display, False);
    std::optional<Window> result;
    if (!g_x_error && status == Success && actual_type == XA_WINDOW && actual_format == 32 &&
        count == 1 && data != nullptr) {
        result = *reinterpret_cast<Window *>(data);
    }
    if (data != nullptr) {
        XFree(data);
    }
    return result;
}

bool Contains(const std::vector<Atom> &atoms, Atom wanted) {
    return wanted != None && std::find(atoms.begin(), atoms.end(), wanted) != atoms.end();
}

struct Ewmh {
    Display *display = nullptr;
    Window root = None;
    bool present = false;
    std::vector<Atom> supported;
    Atom pid = None, wm_state = None, net_wm_state = None, hidden = None, max_vert = None,
         max_horz = None, active = None, close = None, client_list = None, wm_change_state = None;
};

Ewmh LoadEwmh(Display *display) {
    Ewmh ewmh;
    ewmh.display = display;
    ewmh.root = DefaultRootWindow(display);
    auto atom = [&](const char *name) { return XInternAtom(display, name, False); };
    ewmh.pid = atom("_NET_WM_PID");
    ewmh.wm_state = atom("WM_STATE");
    ewmh.net_wm_state = atom("_NET_WM_STATE");
    ewmh.hidden = atom("_NET_WM_STATE_HIDDEN");
    ewmh.max_vert = atom("_NET_WM_STATE_MAXIMIZED_VERT");
    ewmh.max_horz = atom("_NET_WM_STATE_MAXIMIZED_HORZ");
    ewmh.active = atom("_NET_ACTIVE_WINDOW");
    ewmh.close = atom("_NET_CLOSE_WINDOW");
    ewmh.client_list = atom("_NET_CLIENT_LIST");
    ewmh.wm_change_state = atom("WM_CHANGE_STATE");
    // A compliant window manager names a child window that repeats the property.
    const Atom check = atom("_NET_SUPPORTING_WM_CHECK");
    const auto wm = WindowProperty(display, ewmh.root, check);
    if (wm.has_value()) {
        const auto again = WindowProperty(display, wm.value(), check);
        ewmh.present = again.has_value() && again.value() == wm.value();
    }
    if (ewmh.present) {
        ewmh.supported = AtomList(display, ewmh.root, atom("_NET_SUPPORTED"));
    }
    return ewmh;
}

std::optional<unsigned long> OwnPid(Display *display, Window window, Atom pid_atom) {
    Atom actual_type = None;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long bytes_after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const int status = XGetWindowProperty(display, window, pid_atom, 0, 1, False, XA_CARDINAL,
                                          &actual_type, &actual_format, &count, &bytes_after,
                                          &data);
    XSync(display, False);
    std::optional<unsigned long> result;
    if (!g_x_error && status == Success && actual_type == XA_CARDINAL && actual_format == 32 &&
        count == 1 && data != nullptr) {
        result = *reinterpret_cast<unsigned long *>(data);
    }
    if (data != nullptr) {
        XFree(data);
    }
    return result;
}

// ICCCM WM_STATE: 1 normal, 3 iconic; nullopt when the window is unmanaged.
std::optional<long> IcccmState(const Ewmh &ewmh, Window window) {
    Atom actual_type = None;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long bytes_after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const int status = XGetWindowProperty(ewmh.display, window, ewmh.wm_state, 0, 2, False,
                                          ewmh.wm_state, &actual_type, &actual_format, &count,
                                          &bytes_after, &data);
    XSync(ewmh.display, False);
    std::optional<long> result;
    if (!g_x_error && status == Success && actual_format == 32 && count >= 1 && data != nullptr) {
        result = *reinterpret_cast<long *>(data);
    }
    if (data != nullptr) {
        XFree(data);
    }
    return result;
}

void CollectClients(Display *display, Window window, int depth, std::vector<Window> *out) {
    Window root = None;
    Window parent = None;
    Window *children = nullptr;
    unsigned int count = 0;
    g_x_error = false;
    const Status status = XQueryTree(display, window, &root, &parent, &children, &count);
    XSync(display, False);
    if (!status || g_x_error) {
        return;
    }
    for (unsigned int index = 0; index < count && out->size() < 4096; ++index) {
        out->push_back(children[index]);
        if (depth < 3) {
            CollectClients(display, children[index], depth + 1, out);
        }
    }
    if (children != nullptr) {
        XFree(children);
    }
}

struct Geometry {
    int x = 0, y = 0, width = 0, height = 0;
};

std::optional<Geometry> RootGeometry(Display *display, Window window) {
    XWindowAttributes attributes{};
    g_x_error = false;
    if (!XGetWindowAttributes(display, window, &attributes) || g_x_error) {
        return std::nullopt;
    }
    int x = 0;
    int y = 0;
    Window child = None;
    if (!XTranslateCoordinates(display, window, DefaultRootWindow(display), 0, 0, &x, &y,
                               &child)) {
        return std::nullopt;
    }
    XSync(display, False);
    if (g_x_error) {
        return std::nullopt;
    }
    return Geometry{x, y, attributes.width, attributes.height};
}

bool Exists(Display *display, Window window) {
    XWindowAttributes attributes{};
    g_x_error = false;
    const Status status = XGetWindowAttributes(display, window, &attributes);
    XSync(display, False);
    return status && !g_x_error;
}

// The one top-level client window of `pid` at `want` (the AT-SPI frame's
// screen extents). With a single client window of that process its geometry
// may be stale (e.g. while minimized), so it is accepted alone.
std::optional<Window> FindClientWindow(const Ewmh &ewmh, unsigned long pid, const Geometry &want,
                                       std::string *problem) {
    std::vector<Window> all;
    if (ewmh.present) {
        Atom actual_type = None;
        int actual_format = 0;
        unsigned long count = 0;
        unsigned long bytes_after = 0;
        unsigned char *data = nullptr;
        g_x_error = false;
        if (XGetWindowProperty(ewmh.display, ewmh.root, ewmh.client_list, 0, 4096, False,
                               XA_WINDOW, &actual_type, &actual_format, &count, &bytes_after,
                               &data) == Success &&
            actual_type == XA_WINDOW && actual_format == 32 && data != nullptr) {
            const auto *windows = reinterpret_cast<Window *>(data);
            all.assign(windows, windows + count);
        }
        if (data != nullptr) {
            XFree(data);
        }
        XSync(ewmh.display, False);
    }
    if (all.empty()) {
        CollectClients(ewmh.display, ewmh.root, 0, &all);
    }
    std::vector<Window> owned;
    for (const Window window : all) {
        if (OwnPid(ewmh.display, window, ewmh.pid) != pid) {
            continue;
        }
        XWindowAttributes attributes{};
        g_x_error = false;
        if (!XGetWindowAttributes(ewmh.display, window, &attributes) || g_x_error ||
            attributes.override_redirect || attributes.c_class != InputOutput) {
            continue;
        }
        const bool managed = IcccmState(ewmh, window).has_value();
        if (!managed && attributes.map_state != IsViewable) {
            continue;
        }
        owned.push_back(window);
    }
    std::vector<Window> exact;
    for (const Window window : owned) {
        const auto geometry = RootGeometry(ewmh.display, window);
        if (geometry.has_value() && std::abs(geometry->x - want.x) <= 2 &&
            std::abs(geometry->y - want.y) <= 2 && geometry->width == want.width &&
            geometry->height == want.height) {
            exact.push_back(window);
        }
    }
    if (exact.size() == 1) {
        return exact[0];
    }
    if (exact.empty() && owned.size() == 1) {
        return owned[0];
    }
    *problem = owned.empty() ? "window_not_found" : "window_ambiguous";
    return std::nullopt;
}

struct WindowState {
    bool exists = false, minimized = false, maximized = false, active = false;
};

WindowState ReadWindowState(const Ewmh &ewmh, Window window) {
    WindowState state;
    state.exists = Exists(ewmh.display, window);
    if (!state.exists) {
        return state;
    }
    const auto atoms = AtomList(ewmh.display, window, ewmh.net_wm_state);
    const auto icccm = IcccmState(ewmh, window);
    state.minimized = Contains(atoms, ewmh.hidden) || (icccm.has_value() && icccm.value() == 3);
    state.maximized = Contains(atoms, ewmh.max_vert) && Contains(atoms, ewmh.max_horz);
    const auto active = WindowProperty(ewmh.display, ewmh.root, ewmh.active);
    state.active = active.has_value() && active.value() == window;
    return state;
}

std::string StateJson(const WindowState &state) {
    std::string out = "\"exists\":";
    out += state.exists ? "true" : "false";
    out += ",\"minimized\":";
    out += state.minimized ? "true" : "false";
    out += ",\"maximized\":";
    out += state.maximized ? "true" : "false";
    out += ",\"active\":";
    out += state.active ? "true" : "false";
    return out;
}

void SendRootMessage(const Ewmh &ewmh, Window window, Atom type, long l0, long l1, long l2) {
    XEvent event{};
    event.xclient.type = ClientMessage;
    event.xclient.window = window;
    event.xclient.message_type = type;
    event.xclient.format = 32;
    event.xclient.data.l[0] = l0;
    event.xclient.data.l[1] = l1;
    event.xclient.data.l[2] = l2;
    event.xclient.data.l[3] = 1;  // source indication: a normal application
    XSendEvent(ewmh.display, ewmh.root, False, SubstructureRedirectMask | SubstructureNotifyMask,
               &event);
}

struct WindowArgs {
    std::uint64_t pid = 0;
    std::uint64_t deadline_ns = 0;
    Geometry geometry;
    std::string action;
};

std::optional<WindowArgs> ParseWindowArgs(int argc, char **argv, bool action) {
    std::vector<std::string_view> allowed = {"expected-pid", "x", "y", "width", "height",
                                             "deadline-monotonic-ns"};
    if (action) {
        allowed.push_back("action");
    }
    const auto pairs = ParsePairs(argc, argv, allowed);
    if (!pairs.has_value() || pairs->size() != allowed.size()) {
        return std::nullopt;
    }
    WindowArgs parsed;
    const auto pid = ParseUnsigned(Pair(*pairs, "expected-pid").value_or(""));
    const auto deadline = ParseUnsigned(Pair(*pairs, "deadline-monotonic-ns").value_or(""));
    if (!pid.has_value() || pid.value() == 0 || pid.value() > static_cast<std::uint64_t>(INT_MAX) ||
        !deadline.has_value() ||
        !ParseInt(Pair(*pairs, "x").value_or(""), &parsed.geometry.x) ||
        !ParseInt(Pair(*pairs, "y").value_or(""), &parsed.geometry.y) ||
        !ParseInt(Pair(*pairs, "width").value_or(""), &parsed.geometry.width) ||
        !ParseInt(Pair(*pairs, "height").value_or(""), &parsed.geometry.height) ||
        parsed.geometry.width <= 0 || parsed.geometry.height <= 0) {
        return std::nullopt;
    }
    parsed.pid = pid.value();
    parsed.deadline_ns = deadline.value();
    if (action) {
        parsed.action = Pair(*pairs, "action").value_or("");
        if (parsed.action != "minimize" && parsed.action != "maximize" &&
            parsed.action != "restore" && parsed.action != "close" &&
            parsed.action != "activate") {
            return std::nullopt;
        }
    }
    return parsed;
}

// Shared by every new command: the qualified session and an open display.
Display *OpenQualifiedDisplay(std::optional<std::uint64_t> pid, int *exit_code) {
    if (!SessionIsQualified()) {
        EmitError("unsupported_session", "session_preflight", false);
        *exit_code = kExitUnavailable;
        return nullptr;
    }
    if (pid.has_value() && !PidBelongsToCurrentUser(static_cast<unsigned long>(pid.value()))) {
        EmitError("untrusted_process", "target_preflight", false);
        *exit_code = kExitTargetMismatch;
        return nullptr;
    }
    XSetErrorHandler(HandleXError);
    Display *display = XOpenDisplay(nullptr);
    if (display == nullptr) {
        EmitError("display_unavailable", "x11_preflight", false);
        *exit_code = kExitUnavailable;
    }
    return display;
}

int RunWindow(std::string_view command, int argc, char **argv) {
    const bool action = command == "window-action";
    const auto args = ParseWindowArgs(argc, argv, action);
    if (!args.has_value()) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    int exit_code = 0;
    Display *display = OpenQualifiedDisplay(args->pid, &exit_code);
    if (display == nullptr) {
        return exit_code;
    }
    const Ewmh ewmh = LoadEwmh(display);
    std::string problem;
    const auto window = FindClientWindow(ewmh, static_cast<unsigned long>(args->pid),
                                         args->geometry, &problem);
    if (!window.has_value()) {
        XCloseDisplay(display);
        EmitError(problem, "target_preflight", false);
        return kExitTargetMismatch;
    }
    const WindowState before = ReadWindowState(ewmh, window.value());
    const bool maximize = Contains(ewmh.supported, ewmh.net_wm_state) &&
                          Contains(ewmh.supported, ewmh.max_vert) &&
                          Contains(ewmh.supported, ewmh.max_horz);
    const bool activate = Contains(ewmh.supported, ewmh.active);
    const bool close = Contains(ewmh.supported, ewmh.close);
    if (!action) {
        std::cout << "{\"ok\":true,\"window\":" << window.value()
                  << ",\"ewmh\":" << (ewmh.present ? "true" : "false")
                  << ",\"supports\":{\"minimize\":" << (ewmh.present ? "true" : "false")
                  << ",\"maximize\":" << (maximize ? "true" : "false")
                  << ",\"activate\":" << (activate ? "true" : "false")
                  << ",\"close\":" << (close ? "true" : "false") << "}," << StateJson(before)
                  << "}" << std::endl;
        XCloseDisplay(display);
        return 0;
    }
    const std::string &name = args->action;
    const bool supported = ewmh.present && (name == "minimize" || (name == "maximize" && maximize) ||
                                            (name == "activate" && activate) ||
                                            (name == "close" && close) ||
                                            (name == "restore" && (activate || maximize)));
    if (!supported) {
        XCloseDisplay(display);
        EmitError("window_manager_unsupported", "target_preflight", false);
        return kExitUnavailable;
    }
    if (DeadlineExpired(args->deadline_ns)) {
        XCloseDisplay(display);
        EmitError("deadline_exceeded", "pre_dispatch", false);
        return kExitTimeoutBeforeDispatch;
    }
    std::cout << "{\"event\":\"dispatch_started\"}" << std::endl;
    g_x_error = false;
    const Window target = window.value();
    bool restoring_minimized = false;
    if (name == "minimize") {
        XIconifyWindow(display, target, DefaultScreen(display));
    } else if (name == "maximize") {
        SendRootMessage(ewmh, target, ewmh.net_wm_state, 1, static_cast<long>(ewmh.max_vert),
                        static_cast<long>(ewmh.max_horz));
    } else if (name == "restore") {
        if (before.minimized) {
            restoring_minimized = true;
            if (activate) {
                SendRootMessage(ewmh, target, ewmh.active, 1, CurrentTime, 0);
            } else {
                XMapRaised(display, target);
            }
        } else {
            SendRootMessage(ewmh, target, ewmh.net_wm_state, 0, static_cast<long>(ewmh.max_vert),
                            static_cast<long>(ewmh.max_horz));
        }
    } else if (name == "close") {
        SendRootMessage(ewmh, target, ewmh.close, CurrentTime, 2, 0);
    } else {
        SendRootMessage(ewmh, target, ewmh.active, 1, CurrentTime, 0);
    }
    XSync(display, False);
    const bool send_error = g_x_error;
    // Wait (bounded) until the window manager reports the requested state.
    const auto until = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    WindowState after = before;
    bool reached = false;
    while (!send_error) {
        after = ReadWindowState(ewmh, target);
        if (name == "minimize") {
            reached = after.minimized;
        } else if (name == "maximize") {
            reached = after.maximized;
        } else if (name == "restore") {
            reached = after.exists &&
                      (restoring_minimized ? !after.minimized : !after.maximized);
        } else if (name == "close") {
            XWindowAttributes attributes{};
            g_x_error = false;
            const bool alive = XGetWindowAttributes(display, target, &attributes) && !g_x_error;
            reached = !alive || (attributes.map_state == IsUnmapped && !after.minimized);
        } else {
            reached = after.active;
        }
        if (reached || DeadlineExpired(args->deadline_ns) ||
            std::chrono::steady_clock::now() >= until) {
            break;
        }
        usleep(20'000);
    }
    XCloseDisplay(display);
    if (send_error) {
        EmitError("x11_error", "window_dispatch", true);
        return kExitUnknownEffect;
    }
    std::cout << "{\"ok\":true,\"dispatch_started\":true,\"events\":1,\"window\":" << target
              << ",\"action\":\"" << name << "\",\"reached\":" << (reached ? "true" : "false")
              << "," << StateJson(after) << "}" << std::endl;
    return 0;
}

// --- selections ------------------------------------------------------------

struct Selection {
    Display *display = nullptr;
    Window window = None;
    Atom clipboard = None, targets = None, utf8 = None, incr = None, property = None,
         text = None, plain_utf8 = None, plain = None, multiple = None, timestamp = None,
         password_hint = None;
};

Selection OpenSelection(Display *display) {
    Selection selection;
    selection.display = display;
    selection.window =
        XCreateSimpleWindow(display, DefaultRootWindow(display), -10, -10, 1, 1, 0, 0, 0);
    XSelectInput(display, selection.window, PropertyChangeMask);
    auto atom = [&](const char *name) { return XInternAtom(display, name, False); };
    selection.clipboard = atom("CLIPBOARD");
    selection.targets = atom("TARGETS");
    selection.utf8 = atom("UTF8_STRING");
    selection.incr = atom("INCR");
    selection.property = atom("AAD_SELECTION");
    selection.text = atom("TEXT");
    selection.plain_utf8 = atom("text/plain;charset=utf-8");
    selection.plain = atom("text/plain");
    selection.multiple = atom("MULTIPLE");
    selection.timestamp = atom("TIMESTAMP");
    // KDE's (and KeePassXC's) marker for content a clipboard manager must not keep.
    selection.password_hint = atom("x-kde-passwordManagerHint");
    return selection;
}

// Wait for one event of `type` on our window, polling the connection.
bool WaitEvent(Display *display, Window window, int type, XEvent *event,
               std::uint64_t deadline_ns, int max_ms) {
    const auto until = std::chrono::steady_clock::now() + std::chrono::milliseconds(max_ms);
    while (!DeadlineExpired(deadline_ns) && std::chrono::steady_clock::now() < until) {
        if (XCheckTypedWindowEvent(display, window, type, event)) {
            return true;
        }
        pollfd fds[1] = {{ConnectionNumber(display), POLLIN, 0}};
        poll(fds, 1, 10);
    }
    return false;
}

struct Converted {
    Atom type = None;
    int format = 8;
    std::string bytes;
    bool truncated = false;
    bool incremental = false;
};

std::optional<Converted> Convert(const Selection &selection, Atom target, std::size_t limit,
                                 std::uint64_t deadline_ns) {
    XDeleteProperty(selection.display, selection.window, selection.property);
    XConvertSelection(selection.display, selection.clipboard, target, selection.property,
                      selection.window, CurrentTime);
    XFlush(selection.display);
    XEvent event{};
    if (!WaitEvent(selection.display, selection.window, SelectionNotify, &event, deadline_ns,
                   2000) ||
        event.xselection.property == None) {
        return std::nullopt;
    }
    Converted converted;
    Atom actual_type = None;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long bytes_after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const long longs = static_cast<long>(limit / 4 + 1);
    const int status = XGetWindowProperty(selection.display, selection.window, selection.property,
                                          0, longs, True, AnyPropertyType, &actual_type,
                                          &actual_format, &count, &bytes_after, &data);
    XSync(selection.display, False);
    if (g_x_error || status != Success) {
        if (data != nullptr) XFree(data);
        return std::nullopt;
    }
    converted.type = actual_type;
    converted.format = actual_format;
    if (actual_type == selection.incr) {
        converted.incremental = true;
    } else if (data != nullptr) {
        const std::size_t unit = actual_format == 32 ? sizeof(long)
                                 : actual_format == 16 ? sizeof(short)
                                                       : 1;
        converted.bytes.assign(reinterpret_cast<char *>(data), count * unit);
        converted.truncated = bytes_after > 0;
    }
    if (data != nullptr) {
        XFree(data);
    }
    return converted;
}

std::vector<Atom> TargetsOf(const Selection &selection, std::uint64_t deadline_ns) {
    std::vector<Atom> result;
    const auto converted = Convert(selection, selection.targets, 64 * 1024, deadline_ns);
    if (converted.has_value() && converted->format == 32 && !converted->incremental) {
        const auto *atoms = reinterpret_cast<const Atom *>(converted->bytes.data());
        result.assign(atoms, atoms + converted->bytes.size() / sizeof(Atom));
    }
    return result;
}

std::string Latin1ToUtf8(const std::string &latin1) {
    std::string out;
    for (const char item : latin1) {
        const auto code = static_cast<unsigned char>(item);
        if (code < 0x80) {
            out += item;
        } else {
            out += static_cast<char>(0xc0 | (code >> 6));
            out += static_cast<char>(0x80 | (code & 0x3f));
        }
    }
    return out;
}

// `--<name> N --deadline-monotonic-ns D`, both required; N in 1..maximum.
bool ParseLimitAndDeadline(int argc, char **argv, std::string_view name, std::uint64_t maximum,
                           std::uint64_t *value, std::uint64_t *deadline) {
    const auto pairs = ParsePairs(argc, argv, {name, "deadline-monotonic-ns"});
    if (!pairs.has_value() || pairs->size() != 2) {
        return false;
    }
    const auto parsed = ParseUnsigned(Pair(*pairs, name).value_or(""));
    const auto until = ParseUnsigned(Pair(*pairs, "deadline-monotonic-ns").value_or(""));
    if (!parsed.has_value() || !until.has_value() || parsed.value() < 1 ||
        parsed.value() > maximum) {
        return false;
    }
    *value = parsed.value();
    *deadline = until.value();
    return true;
}

int RunClipboardRead(int argc, char **argv) {
    std::uint64_t limit_value = 0;
    std::uint64_t deadline_value = 0;
    if (!ParseLimitAndDeadline(argc, argv, "max-bytes", 1u << 20, &limit_value,
                               &deadline_value)) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const std::optional<std::size_t> limit = static_cast<std::size_t>(limit_value);
    const std::optional<std::uint64_t> deadline = deadline_value;
    int exit_code = 0;
    Display *display = OpenQualifiedDisplay(std::nullopt, &exit_code);
    if (display == nullptr) {
        return exit_code;
    }
    const Selection selection = OpenSelection(display);
    auto finish = [&](const std::string &line, int code) {
        XDestroyWindow(display, selection.window);
        XCloseDisplay(display);
        std::cout << line << std::endl;
        return code;
    };
    if (XGetSelectionOwner(display, selection.clipboard) == None) {
        return finish("{\"ok\":true,\"has_text\":false,\"text\":\"\",\"truncated\":false}", 0);
    }
    const auto targets = TargetsOf(selection, deadline.value());
    if (Contains(targets, selection.password_hint)) {
        return finish("{\"ok\":false,\"code\":\"clipboard_protected\",\"phase\":\"read\","
                      "\"dispatch_started\":false}",
                      kExitProtected);
    }
    const bool utf8 = targets.empty() || Contains(targets, selection.utf8);
    const auto converted =
        Convert(selection, utf8 ? selection.utf8 : XA_STRING, limit.value(), deadline.value());
    if (!converted.has_value() || converted->format != 8) {
        if (DeadlineExpired(deadline.value())) {
            return finish("{\"ok\":false,\"code\":\"deadline_exceeded\",\"phase\":\"read\","
                          "\"dispatch_started\":false}",
                          kExitTimeoutBeforeDispatch);
        }
        return finish("{\"ok\":true,\"has_text\":false,\"text\":\"\",\"truncated\":false}", 0);
    }
    if (converted->incremental) {
        return finish("{\"ok\":false,\"code\":\"clipboard_too_large\",\"phase\":\"read\","
                      "\"dispatch_started\":false}",
                      kExitTooLarge);
    }
    const std::string text = utf8 ? converted->bytes : Latin1ToUtf8(converted->bytes);
    return finish(std::string("{\"ok\":true,\"has_text\":true,\"text\":") + JsonString(text) +
                      ",\"truncated\":" + (converted->truncated ? "true" : "false") + "}",
                  0);
}

// A server timestamp for selection ownership (ICCCM forbids CurrentTime).
Time ServerTime(const Selection &selection) {
    const Atom stamp = XInternAtom(selection.display, "AAD_TIMESTAMP", False);
    unsigned char zero = 0;
    XChangeProperty(selection.display, selection.window, stamp, XA_STRING, 8, PropModeAppend,
                    &zero, 0);
    XFlush(selection.display);
    XEvent event{};
    if (WaitEvent(selection.display, selection.window, PropertyNotify, &event,
                  std::numeric_limits<std::uint64_t>::max(), 1000)) {
        return event.xproperty.time;
    }
    return CurrentTime;
}

struct Item {
    Atom target = None;
    Atom type = None;
    int format = 8;
    std::string bytes;
};

void Reply(Display *display, const XSelectionRequestEvent &request, Atom property) {
    XEvent reply{};
    reply.xselection.type = SelectionNotify;
    reply.xselection.display = display;
    reply.xselection.requestor = request.requestor;
    reply.xselection.selection = request.selection;
    reply.xselection.target = request.target;
    reply.xselection.property = property;
    reply.xselection.time = request.time;
    g_x_error = false;
    XSendEvent(display, request.requestor, False, 0, &reply);
    XSync(display, False);
}

// Answer one request from `items`; returns whether a non-TARGETS item was given.
bool Serve(const Selection &selection, const XSelectionRequestEvent &request,
           const std::vector<Item> &items) {
    const Atom property = request.property == None ? request.target : request.property;
    g_x_error = false;
    if (request.target == selection.targets) {
        std::vector<Atom> atoms = {selection.targets};
        for (const auto &item : items) {
            atoms.push_back(item.target);
        }
        XChangeProperty(selection.display, request.requestor, property, XA_ATOM, 32,
                        PropModeReplace, reinterpret_cast<unsigned char *>(atoms.data()),
                        static_cast<int>(atoms.size()));
        Reply(selection.display, request, g_x_error ? None : property);
        return false;
    }
    for (const auto &item : items) {
        if (item.target != request.target) {
            continue;
        }
        const std::size_t unit = item.format == 32 ? sizeof(long)
                                 : item.format == 16 ? sizeof(short)
                                                     : 1;
        XChangeProperty(selection.display, request.requestor, property, item.type, item.format,
                        PropModeReplace,
                        reinterpret_cast<const unsigned char *>(item.bytes.data()),
                        static_cast<int>(item.bytes.size() / unit));
        XSync(selection.display, False);
        const bool ok = !g_x_error;
        Reply(selection.display, request, ok ? property : None);
        return ok;
    }
    Reply(selection.display, request, None);
    return false;
}

// The previous clipboard's contents, as many targets as fit in one request
// each; nullopt (with *problem) when they cannot all be kept.
std::optional<std::vector<Item>> SaveClipboard(const Selection &selection,
                                               std::uint64_t deadline_ns, std::string *problem) {
    std::vector<Item> saved;
    const std::size_t max_request =
        static_cast<std::size_t>(XExtendedMaxRequestSize(selection.display) != 0
                                     ? XExtendedMaxRequestSize(selection.display)
                                     : XMaxRequestSize(selection.display)) *
            4 -
        256;
    std::size_t total = 0;
    for (const Atom target : TargetsOf(selection, deadline_ns)) {
        if (target == selection.targets || target == selection.multiple ||
            target == selection.timestamp || target == None) {
            continue;
        }
        char *raw = XGetAtomName(selection.display, target);
        const std::string name = raw == nullptr ? "" : raw;
        if (raw != nullptr) XFree(raw);
        if (name == "SAVE_TARGETS" || name == "DELETE" || name.rfind("INSERT_", 0) == 0) {
            continue;
        }
        const auto converted = Convert(selection, target, max_request, deadline_ns);
        if (!converted.has_value()) {
            continue;
        }
        if (converted->incremental || converted->truncated) {
            *problem = "clipboard_too_large";
            return std::nullopt;
        }
        total += converted->bytes.size();
        if (total > kMaxSavedClipboardBytes) {
            *problem = "clipboard_too_large";
            return std::nullopt;
        }
        saved.push_back({target, converted->type, converted->format, converted->bytes});
    }
    return saved;
}

// Own CLIPBOARD with the saved contents from a detached process until another
// client takes it (or an hour passes), so the person's clipboard survives.
bool StartRestorer(const std::vector<Item> &saved, int inherited_display_fd) {
    int ready[2];
    if (pipe(ready) != 0) {
        return false;
    }
    const pid_t child = fork();
    if (child < 0) {
        close(ready[0]);
        close(ready[1]);
        return false;
    }
    if (child == 0) {
        close(ready[0]);
        if (fork() != 0) {
            _exit(0);  // the grandchild is reparented and never waited for
        }
        // Never touch the parent's X connection; just let go of its socket.
        close(inherited_display_fd);
        const int null = open("/dev/null", O_RDWR);
        if (null >= 0) {
            dup2(null, 0);
            dup2(null, 1);
            dup2(null, 2);
        }
        Display *display = XOpenDisplay(nullptr);
        if (display == nullptr) {
            _exit(1);
        }
        const Selection selection = OpenSelection(display);
        XSelectInput(display, selection.window, PropertyChangeMask);
        const Time time = ServerTime(selection);
        XSetSelectionOwner(display, selection.clipboard, selection.window, time);
        const bool owned = XGetSelectionOwner(display, selection.clipboard) == selection.window;
        const char signal = owned ? '1' : '0';
        if (write(ready[1], &signal, 1) < 0) {
            _exit(1);
        }
        close(ready[1]);
        const auto until = std::chrono::steady_clock::now() + std::chrono::hours(1);
        while (owned && std::chrono::steady_clock::now() < until) {
            XEvent event{};
            while (XPending(display) > 0) {
                XNextEvent(display, &event);
                if (event.type == SelectionClear) {
                    _exit(0);
                }
                if (event.type == SelectionRequest) {
                    Serve(selection, event.xselectionrequest, saved);
                }
            }
            pollfd fds[1] = {{ConnectionNumber(display), POLLIN, 0}};
            poll(fds, 1, 500);
        }
        _exit(0);
    }
    close(ready[1]);
    int status = 0;
    waitpid(child, &status, 0);
    char signal = '0';
    pollfd fds[1] = {{ready[0], POLLIN, 0}};
    const bool answered = poll(fds, 1, 2000) > 0 && read(ready[0], &signal, 1) == 1;
    close(ready[0]);
    return answered && signal == '1';
}

std::optional<std::vector<KeyCode>> ChordCodes(Display *display,
                                               const std::vector<std::string> &keys) {
    std::vector<KeyCode> chord;
    bool seen_main = false;
    for (std::size_t index = 0; index < keys.size(); ++index) {
        const KeySym symbol = KeyNamed(keys[index]);
        const KeyCode code = symbol == NoSymbol ? 0 : XKeysymToKeycode(display, symbol);
        const bool last = index + 1 == keys.size();
        if (code == 0 || IsModifier(symbol) == last || seen_main) {
            return std::nullopt;
        }
        seen_main = last;
        chord.push_back(code);
    }
    return chord;
}

bool PressChord(Display *display, const std::vector<KeyCode> &chord) {
    bool submitted = true;
    g_x_error = false;
    for (const KeyCode code : chord) {
        submitted = XTestFakeKeyEvent(display, code, True, 1) != 0 && submitted;
    }
    for (auto code = chord.rbegin(); code != chord.rend(); ++code) {
        submitted = XTestFakeKeyEvent(display, *code, False, 1) != 0 && submitted;
    }
    XSync(display, False);
    return submitted && !g_x_error;
}

// The resource-id mask the server gave this connection. Every client gets the
// same mask, and all of one client's windows share the bits outside it, so a
// requestor window (Qt and GTK use an unnamed helper window without
// _NET_WM_PID) is tied to the client that owns a window with the expected pid.
// Xlib keeps base and mask in the documented-private display fields; they are
// only trusted when our own window id is consistent with them.
std::optional<XID> ResourceMask(Display *display, Window own_window) {
    const auto *raw = reinterpret_cast<_XPrivDisplay>(display);
    const XID base = raw->private3;
    const XID mask = raw->private4;
    if (mask == 0 || (base & mask) != 0 || (own_window & ~mask) != base) {
        return std::nullopt;
    }
    return mask;
}

// Whether `requestor` belongs to the X client that owns `pid_window`.
bool SameClient(Display *display, Window requestor, Window pid_window,
                std::optional<XID> mask, unsigned long pid, Atom pid_atom) {
    const auto owner = WindowPid(display, requestor, pid_atom);
    if (owner.has_value()) {
        return owner.value() == pid;
    }
    return mask.has_value() && pid_window != None &&
           (requestor & ~mask.value()) == (pid_window & ~mask.value());
}

// The focused window's ancestor that carries the expected pid.
Window FocusedPidWindow(Display *display, unsigned long pid, Atom pid_atom) {
    Window focus = None;
    int revert_to = 0;
    XGetInputFocus(display, &focus, &revert_to);
    Window current = focus;
    for (int depth = 0; depth < 64 && current != None && current != PointerRoot; ++depth) {
        if (OwnPid(display, current, pid_atom) == pid) {
            return current;
        }
        Window root = None;
        Window parent = None;
        Window *children = nullptr;
        unsigned int count = 0;
        g_x_error = false;
        const Status status = XQueryTree(display, current, &root, &parent, &children, &count);
        XSync(display, False);
        if (children != nullptr) {
            XFree(children);
        }
        if (!status || g_x_error || parent == None || current == root) {
            break;
        }
        current = parent;
    }
    return None;
}

int RunPaste(int argc, char **argv) {
    std::uint64_t pid_value = 0;
    std::uint64_t deadline_value = 0;
    if (!ParseLimitAndDeadline(argc, argv, "expected-pid", static_cast<std::uint64_t>(INT_MAX),
                               &pid_value, &deadline_value)) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const std::optional<std::uint64_t> pid = pid_value;
    const std::optional<std::uint64_t> deadline = deadline_value;
    const auto text = ReadStdin(kMaxPasteBytes);
    if (!text.has_value() || text->empty() || !DecodeUtf8(*text).has_value()) {
        EmitError("unsupported_text", "text_preflight", false);
        return kExitUnsupportedText;
    }
    int exit_code = 0;
    Display *display = OpenQualifiedDisplay(pid, &exit_code);
    if (display == nullptr) {
        return exit_code;
    }
    int event_base = 0, error_base = 0, major = 0, minor = 0;
    const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", True);
    if (!XTestQueryExtension(display, &event_base, &error_base, &major, &minor) ||
        pid_atom == None) {
        XCloseDisplay(display);
        EmitError("xtest_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    const KeyCode control = XKeysymToKeycode(display, XK_Control_L);
    const KeyCode v = XKeysymToKeycode(display, XK_v);
    if (control == 0 || v == 0) {
        XCloseDisplay(display);
        EmitError("unsupported_key", "key_preflight", false);
        return kExitInvalid;
    }
    const Selection selection = OpenSelection(display);
    auto fail = [&](std::string_view code, std::string_view phase, bool dispatched,
                    int pre_dispatch_exit) {
        XDestroyWindow(display, selection.window);
        XCloseDisplay(display);
        EmitError(code, phase, dispatched);
        return dispatched ? kExitUnknownEffect : pre_dispatch_exit;
    };
    // Keep what the person had before anything changes.
    const bool had_owner = XGetSelectionOwner(display, selection.clipboard) != None;
    std::vector<Item> saved;
    if (had_owner) {
        std::string problem;
        const auto kept = SaveClipboard(selection, deadline.value(), &problem);
        if (!kept.has_value()) {
            return fail(problem, "clipboard_preflight", false, kExitTooLarge);
        }
        saved = kept.value();
    }
    const std::vector<Item> offer = {
        {selection.utf8, selection.utf8, 8, *text},
        {selection.plain_utf8, selection.plain_utf8, 8, *text},
        {selection.text, selection.utf8, 8, *text},
    };
    const Time time = ServerTime(selection);
    XSetSelectionOwner(display, selection.clipboard, selection.window, time);
    if (XGetSelectionOwner(display, selection.clipboard) != selection.window) {
        return fail("clipboard_busy", "clipboard_preflight", false, kExitUnavailable);
    }
    auto restore = [&]() -> std::string {
        if (!had_owner) {
            XSetSelectionOwner(display, selection.clipboard, None, CurrentTime);
            XSync(display, False);
            return "cleared";
        }
        if (saved.empty()) {
            return "none";
        }
        return StartRestorer(saved, ConnectionNumber(display)) ? "restored" : "lost";
    };
    // The caller has just focused the element; give the focus change a moment.
    const auto focus_until = std::chrono::steady_clock::now() + std::chrono::seconds(1);
    while (!FocusBelongsToPid(display, static_cast<unsigned long>(pid.value()), pid_atom) &&
           !DeadlineExpired(deadline.value()) &&
           std::chrono::steady_clock::now() < focus_until) {
        usleep(5000);
    }
    if (DeadlineExpired(deadline.value()) ||
        !FocusBelongsToPid(display, static_cast<unsigned long>(pid.value()), pid_atom)) {
        restore();
        return fail("focus_owner_mismatch", "target_preflight", false, kExitTargetMismatch);
    }
    const Window pid_window =
        FocusedPidWindow(display, static_cast<unsigned long>(pid.value()), pid_atom);
    const auto mask = ResourceMask(display, selection.window);
    std::cout << "{\"event\":\"dispatch_started\"}" << std::endl;
    if (!PressChord(display, {control, v})) {
        restore();
        return fail("x11_error", "keyboard_dispatch", true, kExitUnknownEffect);
    }
    // Serve the text only to the expected process; wait (bounded) for it.
    bool consumed = false;
    bool lost = false;
    const auto until = std::chrono::steady_clock::now() + std::chrono::seconds(2);
    while (!consumed && !lost && !DeadlineExpired(deadline.value()) &&
           std::chrono::steady_clock::now() < until) {
        while (XPending(display) > 0 && !consumed) {
            XEvent event{};
            XNextEvent(display, &event);
            if (event.type == SelectionClear) {
                lost = true;
            } else if (event.type == SelectionRequest) {
                const auto &request = event.xselectionrequest;
                if (SameClient(display, request.requestor, pid_window, mask,
                               static_cast<unsigned long>(pid.value()), pid_atom)) {
                    consumed = Serve(selection, request, offer);
                } else {
                    Reply(display, request, None);
                }
            }
        }
        pollfd fds[1] = {{ConnectionNumber(display), POLLIN, 0}};
        poll(fds, 1, 10);
    }
    // Let the target finish reading before the selection changes hands.
    usleep(50'000);
    const std::string restored = restore();
    XDestroyWindow(display, selection.window);
    XCloseDisplay(display);
    std::cout << "{\"ok\":true,\"dispatch_started\":true,\"events\":4,\"consumed\":"
              << (consumed ? "true" : "false") << ",\"restored\":\"" << restored << "\"}"
              << std::endl;
    return 0;
}

int RunGlobalKey(int argc, char **argv) {
    const auto pairs = ParsePairs(argc, argv, {"keys", "deadline-monotonic-ns"});
    if (!pairs.has_value() || pairs->size() != 2) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const std::string raw_keys = Pair(*pairs, "keys").value_or("");
    const auto parsed_deadline =
        ParseUnsigned(Pair(*pairs, "deadline-monotonic-ns").value_or(""));
    if (!parsed_deadline.has_value() || raw_keys.empty() || raw_keys.size() > 256) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const std::optional<std::uint64_t> deadline = parsed_deadline.value();
    const std::string *raw = &raw_keys;
    std::vector<std::string> keys;
    std::size_t start = 0;
    while (start <= raw->size()) {
        const std::size_t end = std::min(raw->find(',', start), raw->size());
        keys.push_back(raw->substr(start, end - start));
        start = end + 1;
    }
    if (keys.empty() || keys.size() > 6) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    int exit_code = 0;
    Display *display = OpenQualifiedDisplay(std::nullopt, &exit_code);
    if (display == nullptr) {
        return exit_code;
    }
    int event_base = 0, error_base = 0, major = 0, minor = 0;
    if (!XTestQueryExtension(display, &event_base, &error_base, &major, &minor)) {
        XCloseDisplay(display);
        EmitError("xtest_unavailable", "x11_preflight", false);
        return kExitUnavailable;
    }
    const auto chord = ChordCodes(display, keys);
    if (!chord.has_value()) {
        XCloseDisplay(display);
        EmitError("unsupported_key", "key_preflight", false);
        return kExitInvalid;
    }
    if (DeadlineExpired(deadline.value())) {
        XCloseDisplay(display);
        EmitError("deadline_exceeded", "pre_dispatch", false);
        return kExitTimeoutBeforeDispatch;
    }
    std::cout << "{\"event\":\"dispatch_started\"}" << std::endl;
    const bool ok = PressChord(display, chord.value());
    XCloseDisplay(display);
    if (!ok) {
        EmitError("x11_error", "keyboard_dispatch", true);
        return kExitUnknownEffect;
    }
    std::cout << "{\"ok\":true,\"dispatch_started\":true,\"events\":" << 2 * chord->size() << "}"
              << std::endl;
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
    if (command == "window-info" || command == "window-action") {
        return RunWindow(command, argc, argv);
    }
    if (command == "clipboard-read") {
        return RunClipboardRead(argc, argv);
    }
    if (command == "paste") {
        return RunPaste(argc, argv);
    }
    if (command == "global-key") {
        return RunGlobalKey(argc, argv);
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
