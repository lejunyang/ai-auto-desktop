#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/keysym.h>
#include <X11/extensions/XTest.h>

#include <algorithm>
#include <cerrno>
#include <chrono>
#include <climits>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <limits>
#include <optional>
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

}  // namespace

int main(int argc, char **argv) {
    if (argc < 2) {
        EmitError("invalid_arguments", "arguments", false);
        return kExitInvalid;
    }
    const std::string_view command(argv[1]);
    std::optional<std::uint64_t> expected_pid;
    std::optional<std::uint64_t> deadline_ns;
    std::optional<int> click_x;
    std::optional<int> click_y;
    if (command == "type-text") {
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
