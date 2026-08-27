#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>

#include <algorithm>
#include <array>
#include <cerrno>
#include <climits>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <limits>
#include <optional>
#include <signal.h>
#include <string>
#include <string_view>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <utility>
#include <vector>

namespace {

constexpr int kExitInvalid = 64;
constexpr int kExitUnavailable = 69;
constexpr int kExitCaptureFailure = 70;
constexpr int kExitTargetMismatch = 73;
constexpr int kExitDeadline = 75;
constexpr int kExitOutputFailure = 76;
constexpr std::uint64_t kMaxPixels = 16ULL * 1024ULL * 1024ULL;
constexpr std::uint64_t kMaxPngBytes = 64ULL * 1024ULL * 1024ULL;
constexpr std::size_t kMaxWindowDepth = 64;
constexpr std::size_t kMaxWindowNodes = 4096;
constexpr std::size_t kMaxMetadataBytes = 2048;

bool g_x_error = false;

int HandleXError(Display *, XErrorEvent *) {
    g_x_error = true;
    return 0;
}

struct Arguments {
    unsigned long expected_pid = 0;
    int x = 0;
    int y = 0;
    int width = 0;
    int height = 0;
    std::uint64_t deadline_ns = 0;
};

struct ProcessIdentity {
    int directory_fd = -1;
    dev_t device = 0;
    ino_t inode = 0;
    uid_t uid = 0;

    ProcessIdentity() = default;
    ProcessIdentity(const ProcessIdentity &) = delete;
    ProcessIdentity &operator=(const ProcessIdentity &) = delete;

    ProcessIdentity(ProcessIdentity &&other) noexcept
        : directory_fd(other.directory_fd),
          device(other.device),
          inode(other.inode),
          uid(other.uid) {
        other.directory_fd = -1;
    }

    ProcessIdentity &operator=(ProcessIdentity &&other) noexcept {
        if (this != &other) {
            if (directory_fd >= 0) {
                close(directory_fd);
            }
            directory_fd = other.directory_fd;
            device = other.device;
            inode = other.inode;
            uid = other.uid;
            other.directory_fd = -1;
        }
        return *this;
    }

    ~ProcessIdentity() {
        if (directory_fd >= 0) {
            close(directory_fd);
        }
    }
};

struct WindowGeometry {
    int x = 0;
    int y = 0;
    unsigned int width = 0;
    unsigned int height = 0;
    unsigned int border_width = 0;
    int depth = 0;
    int map_state = IsUnmapped;
    int window_class = InputOutput;

    bool operator==(const WindowGeometry &other) const {
        return x == other.x && y == other.y && width == other.width &&
               height == other.height && border_width == other.border_width &&
               depth == other.depth && map_state == other.map_state &&
               window_class == other.window_class;
    }
};

struct SceneSnapshot {
    Window root = None;
    Window top_level = None;
    Window pid_window = None;
    unsigned long pid = 0;
    WindowGeometry root_geometry;
    WindowGeometry target_geometry;
    WindowGeometry pid_geometry;
    std::vector<Window> ancestor_chain;
    std::vector<WindowGeometry> ancestor_geometries;

    bool operator==(const SceneSnapshot &other) const {
        return root == other.root && top_level == other.top_level &&
               pid_window == other.pid_window && pid == other.pid &&
               root_geometry == other.root_geometry &&
               target_geometry == other.target_geometry &&
               pid_geometry == other.pid_geometry &&
               ancestor_chain == other.ancestor_chain &&
               ancestor_geometries == other.ancestor_geometries;
    }
};

enum class SceneError {
    kNone,
    kX11,
    kBounds,
    kNoTarget,
    kPidUnavailable,
    kPidMismatch,
    kTargetNotViewable,
    kRegionOutsideTarget,
    kTargetAmbiguous,
    kTargetWindowMismatch,
    kOccluded,
};

struct SceneResult {
    SceneSnapshot snapshot;
    SceneError error = SceneError::kNone;
};

bool WriteAll(int fd, const unsigned char *data, std::size_t size) {
    std::size_t offset = 0;
    while (offset < size) {
        const ssize_t written = write(fd, data + offset, size - offset);
        if (written > 0) {
            offset += static_cast<std::size_t>(written);
            continue;
        }
        if (written < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

void EmitError(std::string_view code, std::string_view phase,
               const Arguments *arguments = nullptr) {
    std::array<char, kMaxMetadataBytes> buffer{};
    int length = 0;
    if (arguments == nullptr) {
        length = std::snprintf(
            buffer.data(), buffer.size(),
            "{\"ok\":false,\"schema_version\":1,\"code\":\"%.*s\","
            "\"phase\":\"%.*s\"}\n",
            static_cast<int>(code.size()), code.data(),
            static_cast<int>(phase.size()), phase.data());
    } else {
        length = std::snprintf(
            buffer.data(), buffer.size(),
            "{\"ok\":false,\"schema_version\":1,\"code\":\"%.*s\","
            "\"phase\":\"%.*s\",\"expected_pid\":%lu,"
            "\"x\":%d,\"y\":%d,\"width\":%d,\"height\":%d}\n",
            static_cast<int>(code.size()), code.data(),
            static_cast<int>(phase.size()), phase.data(),
            arguments->expected_pid, arguments->x, arguments->y,
            arguments->width, arguments->height);
    }
    if (length <= 0 || static_cast<std::size_t>(length) >= buffer.size()) {
        static constexpr unsigned char fallback[] =
            "{\"ok\":false,\"schema_version\":1,\"code\":\"metadata_error\","
            "\"phase\":\"report\"}\n";
        (void)WriteAll(STDERR_FILENO, fallback, sizeof(fallback) - 1);
        return;
    }
    (void)WriteAll(STDERR_FILENO,
                   reinterpret_cast<const unsigned char *>(buffer.data()),
                   static_cast<std::size_t>(length));
}

bool EmitSuccess(const Arguments &arguments, const SceneSnapshot &snapshot,
                 std::size_t png_bytes) {
    std::array<char, kMaxMetadataBytes> buffer{};
    const int length = std::snprintf(
        buffer.data(), buffer.size(),
        "{\"ok\":true,\"schema_version\":1,"
        "\"capture_method\":\"x11_root_xgetimage\","
        "\"format\":\"png\",\"mime_type\":\"image/png\","
        "\"expected_pid\":%lu,\"target_pid\":%lu,"
        "\"target_window\":%lu,\"target_top_level_window\":%lu,"
        "\"root_window\":%lu,\"x\":%d,\"y\":%d,"
        "\"width\":%d,\"height\":%d,\"root_width\":%u,"
        "\"root_height\":%u,\"png_bytes\":%zu,"
        "\"cursor_included\":false,\"occlusion_checked\":true,"
        "\"same_euid_verified\":true,\"scene_stable\":true}\n",
        arguments.expected_pid, snapshot.pid, snapshot.pid_window,
        snapshot.top_level, snapshot.root, arguments.x, arguments.y,
        arguments.width, arguments.height, snapshot.root_geometry.width,
        snapshot.root_geometry.height, png_bytes);
    if (length <= 0 || static_cast<std::size_t>(length) >= buffer.size()) {
        return false;
    }
    return WriteAll(STDERR_FILENO,
                    reinterpret_cast<const unsigned char *>(buffer.data()),
                    static_cast<std::size_t>(length));
}

std::optional<std::uint64_t> ParseUnsigned(std::string_view value) {
    if (value.empty()) {
        return std::nullopt;
    }
    std::uint64_t result = 0;
    for (const char item : value) {
        if (item < '0' || item > '9') {
            return std::nullopt;
        }
        const unsigned int digit = static_cast<unsigned int>(item - '0');
        if (result > (std::numeric_limits<std::uint64_t>::max() - digit) / 10) {
            return std::nullopt;
        }
        result = result * 10 + digit;
    }
    return result;
}

std::optional<int> ParseInt(std::string_view value) {
    if (value.empty()) {
        return std::nullopt;
    }
    bool negative = false;
    std::size_t index = 0;
    if (value.front() == '-') {
        negative = true;
        index = 1;
    }
    if (index == value.size()) {
        return std::nullopt;
    }
    std::uint64_t magnitude = 0;
    const std::uint64_t limit = negative
                                    ? static_cast<std::uint64_t>(INT_MAX) + 1ULL
                                    : static_cast<std::uint64_t>(INT_MAX);
    for (; index < value.size(); ++index) {
        const char item = value[index];
        if (item < '0' || item > '9') {
            return std::nullopt;
        }
        const unsigned int digit = static_cast<unsigned int>(item - '0');
        if (magnitude > (limit - digit) / 10) {
            return std::nullopt;
        }
        magnitude = magnitude * 10 + digit;
    }
    if (negative && magnitude == static_cast<std::uint64_t>(INT_MAX) + 1ULL) {
        return INT_MIN;
    }
    const int result = static_cast<int>(magnitude);
    return negative ? -result : result;
}

std::optional<Arguments> ParseArguments(int argc, char **argv) {
    if (argc != 14 || std::string_view(argv[1]) != "capture-target" ||
        std::string_view(argv[2]) != "--expected-pid" ||
        std::string_view(argv[4]) != "--x" ||
        std::string_view(argv[6]) != "--y" ||
        std::string_view(argv[8]) != "--width" ||
        std::string_view(argv[10]) != "--height" ||
        std::string_view(argv[12]) != "--deadline-monotonic-ns") {
        return std::nullopt;
    }
    const auto pid = ParseUnsigned(argv[3]);
    const auto x = ParseInt(argv[5]);
    const auto y = ParseInt(argv[7]);
    const auto width = ParseInt(argv[9]);
    const auto height = ParseInt(argv[11]);
    const auto deadline = ParseUnsigned(argv[13]);
    if (!pid.has_value() || pid.value() == 0 ||
        pid.value() > static_cast<std::uint64_t>(INT_MAX) || !x.has_value() ||
        !y.has_value() || !width.has_value() || width.value() <= 0 ||
        !height.has_value() || height.value() <= 0 || !deadline.has_value() ||
        deadline.value() == 0) {
        return std::nullopt;
    }
    const std::uint64_t pixels = static_cast<std::uint64_t>(width.value()) *
                                 static_cast<std::uint64_t>(height.value());
    const std::uint64_t raw_bytes =
        static_cast<std::uint64_t>(height.value()) *
        (1ULL + static_cast<std::uint64_t>(width.value()) * 4ULL);
    const std::uint64_t deflate_blocks = (raw_bytes + 65534ULL) / 65535ULL;
    const std::uint64_t worst_png_bytes =
        8ULL + 25ULL + 12ULL + (2ULL + raw_bytes + deflate_blocks * 5ULL + 4ULL)
        + 12ULL;
    if (pixels == 0 || pixels > kMaxPixels || raw_bytes > kMaxPngBytes
        || worst_png_bytes > kMaxPngBytes) {
        return std::nullopt;
    }
    return Arguments{static_cast<unsigned long>(pid.value()), x.value(), y.value(),
                     width.value(), height.value(), deadline.value()};
}

bool DeadlineExpired(std::uint64_t deadline_ns) {
    struct timespec now {};
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || now.tv_sec < 0 ||
        now.tv_nsec < 0) {
        return true;
    }
    const std::uint64_t seconds = static_cast<std::uint64_t>(now.tv_sec);
    if (seconds > std::numeric_limits<std::uint64_t>::max() / 1000000000ULL) {
        return true;
    }
    const std::uint64_t now_ns =
        seconds * 1000000000ULL + static_cast<std::uint64_t>(now.tv_nsec);
    return now_ns >= deadline_ns;
}

bool IsKdeDesktop(std::string_view value) {
    std::size_t start = 0;
    while (start <= value.size()) {
        std::size_t end = value.find_first_of(":;", start);
        if (end == std::string_view::npos) {
            end = value.size();
        }
        std::string token(value.substr(start, end - start));
        std::transform(token.begin(), token.end(), token.begin(), [](char item) {
            if (item >= 'a' && item <= 'z') {
                return static_cast<char>(item - 'a' + 'A');
            }
            return item;
        });
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
    if (session_type == nullptr || display == nullptr || *display == '\0' ||
        desktop == nullptr) {
        return false;
    }
    std::string normalized_type(session_type);
    std::transform(normalized_type.begin(), normalized_type.end(),
                   normalized_type.begin(), [](char item) {
                       if (item >= 'A' && item <= 'Z') {
                           return static_cast<char>(item - 'A' + 'a');
                       }
                       return item;
                   });
    return normalized_type == "x11" && IsKdeDesktop(desktop);
}

std::optional<ProcessIdentity> OpenTrustedProcess(unsigned long pid) {
    const std::string path = "/proc/" + std::to_string(pid);
    const int fd = open(path.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (fd < 0) {
        return std::nullopt;
    }
    struct stat details {};
    if (fstat(fd, &details) != 0 || !S_ISDIR(details.st_mode) ||
        details.st_uid != geteuid()) {
        close(fd);
        return std::nullopt;
    }
    ProcessIdentity result;
    result.directory_fd = fd;
    result.device = details.st_dev;
    result.inode = details.st_ino;
    result.uid = details.st_uid;
    return result;
}

bool ProcessIdentityUnchanged(unsigned long pid, const ProcessIdentity &identity) {
    if (identity.directory_fd < 0 || identity.uid != geteuid()) {
        return false;
    }
    struct stat held {};
    if (fstat(identity.directory_fd, &held) != 0 || held.st_uid != geteuid() ||
        held.st_dev != identity.device || held.st_ino != identity.inode) {
        return false;
    }
    const std::string path = "/proc/" + std::to_string(pid);
    struct stat current {};
    return stat(path.c_str(), &current) == 0 && current.st_uid == geteuid() &&
           current.st_dev == identity.device && current.st_ino == identity.inode;
}

bool XOperationSucceeded(Display *display) {
    XSync(display, False);
    return !g_x_error;
}

std::optional<unsigned long> DirectWindowPid(Display *display, Window window,
                                             Atom pid_atom) {
    Atom actual_type = None;
    int actual_format = 0;
    unsigned long count = 0;
    unsigned long bytes_after = 0;
    unsigned char *data = nullptr;
    g_x_error = false;
    const int status = XGetWindowProperty(
        display, window, pid_atom, 0, 1, False, XA_CARDINAL, &actual_type,
        &actual_format, &count, &bytes_after, &data);
    const bool operation_ok = XOperationSucceeded(display);
    std::optional<unsigned long> result;
    if (operation_ok && status == Success && actual_type == XA_CARDINAL &&
        actual_format == 32 && count == 1 && bytes_after == 0 && data != nullptr) {
        const unsigned long value = *reinterpret_cast<unsigned long *>(data);
        if (value > 0 && value <= static_cast<unsigned long>(INT_MAX)) {
            result = value;
        }
    }
    if (data != nullptr) {
        XFree(data);
    }
    return result;
}

bool GetGeometry(Display *display, Window window, WindowGeometry *geometry) {
    XWindowAttributes attributes {};
    g_x_error = false;
    const Status status = XGetWindowAttributes(display, window, &attributes);
    if (!XOperationSucceeded(display) || status == 0) {
        return false;
    }
    geometry->x = attributes.x;
    geometry->y = attributes.y;
    geometry->width = static_cast<unsigned int>(attributes.width);
    geometry->height = static_cast<unsigned int>(attributes.height);
    geometry->border_width = static_cast<unsigned int>(attributes.border_width);
    geometry->depth = attributes.depth;
    geometry->map_state = attributes.map_state;
    geometry->window_class = attributes.c_class;
    return attributes.width >= 0 && attributes.height >= 0;
}

bool GetRootRelativeGeometry(Display *display, Window window, Window root,
                             WindowGeometry *geometry) {
    if (!GetGeometry(display, window, geometry)) {
        return false;
    }
    Window child = None;
    int root_x = 0;
    int root_y = 0;
    g_x_error = false;
    const Bool translated = XTranslateCoordinates(
        display, window, root, 0, 0, &root_x, &root_y, &child);
    if (!XOperationSucceeded(display) || translated == False) {
        return false;
    }
    geometry->x = root_x;
    geometry->y = root_y;
    return true;
}

bool RegionWithinRoot(const Arguments &arguments,
                      const WindowGeometry &root_geometry) {
    const std::int64_t right = static_cast<std::int64_t>(arguments.x) +
                               static_cast<std::int64_t>(arguments.width);
    const std::int64_t bottom = static_cast<std::int64_t>(arguments.y) +
                                static_cast<std::int64_t>(arguments.height);
    return arguments.x >= 0 && arguments.y >= 0 &&
           right <= static_cast<std::int64_t>(root_geometry.width) &&
           bottom <= static_cast<std::int64_t>(root_geometry.height);
}

bool RectanglesOverlap(std::int64_t left_a, std::int64_t top_a,
                       std::int64_t right_a, std::int64_t bottom_a,
                       std::int64_t left_b, std::int64_t top_b,
                       std::int64_t right_b, std::int64_t bottom_b) {
    return left_a < right_b && left_b < right_a && top_a < bottom_b &&
           top_b < bottom_a;
}

bool RegionWithinTarget(const Arguments &arguments,
                        const WindowGeometry &target_geometry) {
    const std::int64_t region_right = static_cast<std::int64_t>(arguments.x) +
                                      arguments.width;
    const std::int64_t region_bottom = static_cast<std::int64_t>(arguments.y) +
                                       arguments.height;
    const std::int64_t target_right =
        static_cast<std::int64_t>(target_geometry.x) + target_geometry.width;
    const std::int64_t target_bottom =
        static_cast<std::int64_t>(target_geometry.y) + target_geometry.height;
    return arguments.x >= target_geometry.x && arguments.y >= target_geometry.y &&
           region_right <= target_right && region_bottom <= target_bottom;
}

std::optional<std::vector<Window>> WindowsAtPoint(Display *display, Window root,
                                                  int x, int y) {
    std::vector<Window> path;
    path.reserve(8);
    Window parent = root;
    for (std::size_t depth = 0; depth < kMaxWindowDepth; ++depth) {
        Window child = None;
        int translated_x = 0;
        int translated_y = 0;
        g_x_error = false;
        const Bool translated = XTranslateCoordinates(
            display, root, parent, x, y, &translated_x, &translated_y, &child);
        if (!XOperationSucceeded(display) || translated == False) {
            return std::nullopt;
        }
        if (child == None) {
            return path;
        }
        if (child == parent ||
            std::find(path.begin(), path.end(), child) != path.end()) {
            return std::nullopt;
        }
        path.push_back(child);
        parent = child;
    }
    return std::nullopt;
}

std::optional<std::vector<Window>> QueryChildren(Display *display, Window window,
                                                 Window expected_root) {
    Window returned_root = None;
    Window parent = None;
    Window *children = nullptr;
    unsigned int child_count = 0;
    g_x_error = false;
    const Status status = XQueryTree(display, window, &returned_root, &parent,
                                     &children, &child_count);
    const bool operation_ok = XOperationSucceeded(display);
    if (!operation_ok || status == 0 || returned_root != expected_root) {
        if (children != nullptr) {
            XFree(children);
        }
        return std::nullopt;
    }
    if (child_count > kMaxWindowNodes) {
        if (children != nullptr) {
            XFree(children);
        }
        return std::nullopt;
    }
    std::vector<Window> result;
    result.reserve(child_count);
    for (unsigned int index = 0; index < child_count; ++index) {
        result.push_back(children[index]);
    }
    if (children != nullptr) {
        XFree(children);
    }
    return result;
}

bool GeometryOverlapsRegion(const WindowGeometry &geometry,
                            const Arguments &arguments) {
    const std::int64_t border = geometry.border_width;
    const std::int64_t window_left =
        static_cast<std::int64_t>(geometry.x) - border;
    const std::int64_t window_top =
        static_cast<std::int64_t>(geometry.y) - border;
    const std::int64_t window_right =
        static_cast<std::int64_t>(geometry.x) + geometry.width + border;
    const std::int64_t window_bottom =
        static_cast<std::int64_t>(geometry.y) + geometry.height + border;
    const std::int64_t region_right =
        static_cast<std::int64_t>(arguments.x) + arguments.width;
    const std::int64_t region_bottom =
        static_cast<std::int64_t>(arguments.y) + arguments.height;
    return RectanglesOverlap(arguments.x, arguments.y, region_right, region_bottom,
                             window_left, window_top, window_right, window_bottom);
}

struct PidSearchResult {
    std::vector<Window> path;
    std::size_t matches = 0;
};

bool FindExpectedPidDescendant(Display *display, Window window, Window root,
                               Atom pid_atom, unsigned long expected_pid,
                               std::vector<Window> *path,
                               PidSearchResult *result, std::size_t depth,
                               std::size_t *visited) {
    if (depth > kMaxWindowDepth || ++(*visited) > kMaxWindowNodes) {
        return false;
    }
    WindowGeometry geometry;
    if (!GetRootRelativeGeometry(display, window, root, &geometry)) {
        return false;
    }
    if (geometry.map_state != IsViewable) {
        return true;
    }
    path->push_back(window);
    const auto pid = DirectWindowPid(display, window, pid_atom);
    if (pid.has_value() && geometry.window_class == InputOutput) {
        if (pid.value() == expected_pid) {
            ++result->matches;
            if (result->matches == 1) {
                result->path = *path;
            }
        }
    }
    const auto children = QueryChildren(display, window, root);
    if (!children.has_value()) {
        path->pop_back();
        return false;
    }
    for (const Window child : children.value()) {
        if (!FindExpectedPidDescendant(display, child, root, pid_atom,
                                       expected_pid, path, result, depth + 1,
                                       visited)) {
            path->pop_back();
            return false;
        }
    }
    path->pop_back();
    return true;
}

struct TargetBinding {
    Window top_level = None;
    Window pid_window = None;
    std::vector<Window> ancestor_chain;
};

std::optional<TargetBinding> FindUniqueTargetBinding(
    Display *display, Window root, Atom pid_atom, const Arguments &arguments,
    SceneError *error) {
    const auto top_levels = QueryChildren(display, root, root);
    if (!top_levels.has_value()) {
        *error = SceneError::kX11;
        return std::nullopt;
    }
    std::vector<TargetBinding> candidates;
    std::size_t visited = 0;
    for (const Window top_level : top_levels.value()) {
        WindowGeometry geometry;
        if (!GetRootRelativeGeometry(display, top_level, root, &geometry)) {
            *error = SceneError::kX11;
            return std::nullopt;
        }
        if (geometry.map_state != IsViewable ||
            geometry.window_class != InputOutput || geometry.width == 0 ||
            geometry.height == 0 || !RegionWithinTarget(arguments, geometry)) {
            continue;
        }
        PidSearchResult search;
        std::vector<Window> path;
        if (!FindExpectedPidDescendant(
                display, top_level, root, pid_atom, arguments.expected_pid, &path,
                &search, 0, &visited)) {
            *error = SceneError::kX11;
            return std::nullopt;
        }
        if (search.matches > 1) {
            *error = SceneError::kTargetAmbiguous;
            return std::nullopt;
        }
        if (search.matches == 1) {
            candidates.push_back(
                TargetBinding{top_level, search.path.back(), search.path});
            if (candidates.size() > 1) {
                *error = SceneError::kTargetAmbiguous;
                return std::nullopt;
            }
        }
    }
    if (candidates.empty()) {
        *error = SceneError::kNoTarget;
        return std::nullopt;
    }
    *error = SceneError::kNone;
    return std::move(candidates.front());
}

bool IsAncestorChainWindow(Window window,
                           const std::vector<Window> &ancestor_chain) {
    return std::find(ancestor_chain.begin(), ancestor_chain.end(), window) !=
           ancestor_chain.end();
}

SceneError CheckTargetDescendants(
    Display *display, Window window, Window root, const Arguments &arguments,
    const std::vector<Window> &ancestor_chain, std::size_t depth,
    std::size_t *visited) {
    if (depth > kMaxWindowDepth || ++(*visited) > kMaxWindowNodes) {
        return SceneError::kX11;
    }
    const auto children = QueryChildren(display, window, root);
    if (!children.has_value()) {
        return SceneError::kX11;
    }
    for (const Window child : children.value()) {
        WindowGeometry geometry;
        if (!GetRootRelativeGeometry(display, child, root, &geometry)) {
            return SceneError::kX11;
        }
        if (geometry.map_state != IsViewable) {
            continue;
        }
        if (geometry.window_class == InputOutput &&
            !IsAncestorChainWindow(child, ancestor_chain) &&
            GeometryOverlapsRegion(geometry, arguments)) {
            return SceneError::kOccluded;
        }
        const SceneError nested = CheckTargetDescendants(
            display, child, root, arguments, ancestor_chain, depth + 1, visited);
        if (nested != SceneError::kNone) {
            return nested;
        }
    }
    return SceneError::kNone;
}

SceneError CheckTopLevelOcclusion(Display *display, Window root,
                                  Window target_top_level,
                                  const Arguments &arguments) {
    const auto children = QueryChildren(display, root, root);
    if (!children.has_value()) {
        return SceneError::kX11;
    }
    bool found_target = false;
    SceneError result = SceneError::kNone;
    for (const Window child : children.value()) {
        if (child == target_top_level) {
            found_target = true;
            continue;
        }
        if (!found_target) {
            continue;
        }
        WindowGeometry child_geometry;
        if (!GetRootRelativeGeometry(display, child, root, &child_geometry)) {
            result = SceneError::kX11;
            break;
        }
        if (child_geometry.window_class != InputOutput ||
            child_geometry.map_state != IsViewable ||
            child_geometry.width == 0 || child_geometry.height == 0) {
            continue;
        }
        if (GeometryOverlapsRegion(child_geometry, arguments)) {
            result = SceneError::kOccluded;
            break;
        }
    }
    if (result == SceneError::kNone && !found_target) {
        return SceneError::kNoTarget;
    }
    return result;
}

SceneResult InspectScene(Display *display, Atom pid_atom,
                         const Arguments &arguments) {
    SceneResult result;
    result.snapshot.root = DefaultRootWindow(display);
    if (result.snapshot.root == None ||
        !GetGeometry(display, result.snapshot.root,
                     &result.snapshot.root_geometry)) {
        result.error = SceneError::kX11;
        return result;
    }
    if (!RegionWithinRoot(arguments, result.snapshot.root_geometry)) {
        result.error = SceneError::kBounds;
        return result;
    }
    const int center_x = arguments.x + arguments.width / 2;
    const int center_y = arguments.y + arguments.height / 2;
    const auto path =
        WindowsAtPoint(display, result.snapshot.root, center_x, center_y);
    if (!path.has_value()) {
        result.error = SceneError::kX11;
        return result;
    }
    if (path->empty()) {
        result.error = SceneError::kNoTarget;
        return result;
    }
    const Window center_top_level = path->front();
    unsigned long center_pid = 0;
    for (auto item = path->rbegin(); item != path->rend(); ++item) {
        const auto pid = DirectWindowPid(display, *item, pid_atom);
        if (pid.has_value()) {
            center_pid = pid.value();
            break;
        }
    }
    if (center_pid == 0) {
        result.error = SceneError::kPidUnavailable;
        return result;
    }
    if (center_pid != arguments.expected_pid) {
        result.error = SceneError::kPidMismatch;
        return result;
    }
    SceneError binding_error = SceneError::kNone;
    const auto binding = FindUniqueTargetBinding(
        display, result.snapshot.root, pid_atom, arguments, &binding_error);
    if (!binding.has_value()) {
        result.error = binding_error;
        return result;
    }
    if (binding->top_level != center_top_level) {
        result.error = SceneError::kTargetWindowMismatch;
        return result;
    }
    result.snapshot.top_level = binding->top_level;
    result.snapshot.pid_window = binding->pid_window;
    result.snapshot.pid = arguments.expected_pid;
    result.snapshot.ancestor_chain = binding->ancestor_chain;
    if (!GetRootRelativeGeometry(display, result.snapshot.top_level,
                                 result.snapshot.root,
                                 &result.snapshot.target_geometry)) {
        result.error = SceneError::kX11;
        return result;
    }
    if (result.snapshot.target_geometry.map_state != IsViewable ||
        result.snapshot.target_geometry.width == 0 ||
        result.snapshot.target_geometry.height == 0) {
        result.error = SceneError::kTargetNotViewable;
        return result;
    }
    if (!RegionWithinTarget(arguments, result.snapshot.target_geometry)) {
        result.error = SceneError::kRegionOutsideTarget;
        return result;
    }
    if (!GetRootRelativeGeometry(display, result.snapshot.pid_window,
                                 result.snapshot.root,
                                 &result.snapshot.pid_geometry)) {
        result.error = SceneError::kX11;
        return result;
    }
    result.snapshot.ancestor_geometries.reserve(
        result.snapshot.ancestor_chain.size());
    for (const Window ancestor : result.snapshot.ancestor_chain) {
        WindowGeometry geometry;
        if (!GetRootRelativeGeometry(display, ancestor, result.snapshot.root,
                                     &geometry)) {
            result.error = SceneError::kX11;
            return result;
        }
        if (geometry.map_state != IsViewable ||
            geometry.window_class != InputOutput ||
            !RegionWithinTarget(arguments, geometry)) {
            result.error = SceneError::kRegionOutsideTarget;
            return result;
        }
        result.snapshot.ancestor_geometries.push_back(geometry);
    }
    std::size_t visited = 0;
    result.error = CheckTargetDescendants(
        display, result.snapshot.top_level, result.snapshot.root, arguments,
        result.snapshot.ancestor_chain, 0, &visited);
    if (result.error != SceneError::kNone) {
        return result;
    }
    result.error = CheckTopLevelOcclusion(display, result.snapshot.root,
                                          result.snapshot.top_level, arguments);
    return result;
}

std::pair<std::string_view, std::string_view> SceneErrorDetails(SceneError error) {
    switch (error) {
        case SceneError::kX11:
            return {"x11_error", "scene_preflight"};
        case SceneError::kBounds:
            return {"bounds_out_of_root", "bounds_preflight"};
        case SceneError::kNoTarget:
            return {"target_window_unavailable", "target_preflight"};
        case SceneError::kPidUnavailable:
            return {"target_pid_unavailable", "target_preflight"};
        case SceneError::kPidMismatch:
            return {"target_pid_mismatch", "target_preflight"};
        case SceneError::kTargetNotViewable:
            return {"target_not_viewable", "target_preflight"};
        case SceneError::kRegionOutsideTarget:
            return {"region_outside_target", "target_preflight"};
        case SceneError::kTargetAmbiguous:
            return {"target_window_ambiguous", "target_preflight"};
        case SceneError::kTargetWindowMismatch:
            return {"target_window_mismatch", "target_preflight"};
        case SceneError::kOccluded:
            return {"target_occluded", "occlusion_preflight"};
        case SceneError::kNone:
            break;
    }
    return {"unknown_scene_error", "scene_preflight"};
}

std::uint32_t Crc32(const unsigned char *data, std::size_t size) {
    std::uint32_t crc = 0xffffffffU;
    for (std::size_t index = 0; index < size; ++index) {
        crc ^= data[index];
        for (int bit = 0; bit < 8; ++bit) {
            const std::uint32_t mask =
                static_cast<std::uint32_t>(-static_cast<std::int32_t>(crc & 1U));
            crc = (crc >> 1U) ^ (0xedb88320U & mask);
        }
    }
    return ~crc;
}

std::uint32_t Adler32(const std::vector<unsigned char> &data) {
    constexpr std::uint32_t modulus = 65521U;
    std::uint32_t first = 1U;
    std::uint32_t second = 0U;
    std::size_t offset = 0;
    while (offset < data.size()) {
        const std::size_t block = std::min<std::size_t>(5552, data.size() - offset);
        for (std::size_t index = 0; index < block; ++index) {
            first += data[offset + index];
            second += first;
        }
        first %= modulus;
        second %= modulus;
        offset += block;
    }
    return (second << 16U) | first;
}

void AppendBigEndian32(std::vector<unsigned char> *output, std::uint32_t value) {
    output->push_back(static_cast<unsigned char>((value >> 24U) & 0xffU));
    output->push_back(static_cast<unsigned char>((value >> 16U) & 0xffU));
    output->push_back(static_cast<unsigned char>((value >> 8U) & 0xffU));
    output->push_back(static_cast<unsigned char>(value & 0xffU));
}

void AppendChunk(std::vector<unsigned char> *png, const char type[4],
                 const std::vector<unsigned char> &payload) {
    AppendBigEndian32(png, static_cast<std::uint32_t>(payload.size()));
    const std::size_t crc_start = png->size();
    png->insert(png->end(), type, type + 4);
    png->insert(png->end(), payload.begin(), payload.end());
    AppendBigEndian32(png, Crc32(png->data() + crc_start, 4 + payload.size()));
}

bool MaskIsContiguous(unsigned long mask) {
    if (mask == 0) {
        return false;
    }
    while ((mask & 1UL) == 0) {
        mask >>= 1U;
    }
    return (mask & (mask + 1UL)) == 0;
}

unsigned char ScaleChannel(unsigned long pixel, unsigned long mask) {
    unsigned int shift = 0;
    while ((mask & 1UL) == 0) {
        mask >>= 1U;
        ++shift;
    }
    const std::uint64_t maximum = static_cast<std::uint64_t>(mask);
    const std::uint64_t value =
        (static_cast<std::uint64_t>(pixel) >> shift) & maximum;
    return static_cast<unsigned char>((value * 255ULL + maximum / 2ULL) / maximum);
}

std::optional<std::vector<unsigned char>> EncodePng(XImage *image, int width,
                                                    int height) {
    if (image == nullptr || width <= 0 || height <= 0 ||
        !MaskIsContiguous(image->red_mask) ||
        !MaskIsContiguous(image->green_mask) ||
        !MaskIsContiguous(image->blue_mask)) {
        return std::nullopt;
    }
    const std::size_t row_size = 1U + static_cast<std::size_t>(width) * 4U;
    if (row_size > std::numeric_limits<std::size_t>::max() /
                       static_cast<std::size_t>(height)) {
        return std::nullopt;
    }
    std::vector<unsigned char> raw(row_size * static_cast<std::size_t>(height));
    for (int y = 0; y < height; ++y) {
        const std::size_t row = static_cast<std::size_t>(y) * row_size;
        raw[row] = 0;
        for (int x = 0; x < width; ++x) {
            const unsigned long pixel = XGetPixel(image, x, y);
            const std::size_t output = row + 1U + static_cast<std::size_t>(x) * 4U;
            raw[output] = ScaleChannel(pixel, image->red_mask);
            raw[output + 1U] = ScaleChannel(pixel, image->green_mask);
            raw[output + 2U] = ScaleChannel(pixel, image->blue_mask);
            raw[output + 3U] = 255U;
        }
    }

    std::vector<unsigned char> compressed;
    const std::size_t blocks = (raw.size() + 65534U) / 65535U;
    if (raw.size() > std::numeric_limits<std::size_t>::max() - 6U - blocks * 5U) {
        return std::nullopt;
    }
    compressed.reserve(2U + blocks * 5U + raw.size() + 4U);
    compressed.push_back(0x78U);
    compressed.push_back(0x01U);
    std::size_t offset = 0;
    while (offset < raw.size()) {
        const std::size_t remaining = raw.size() - offset;
        const std::uint16_t length = static_cast<std::uint16_t>(
            std::min<std::size_t>(remaining, 65535U));
        const bool final_block = static_cast<std::size_t>(length) == remaining;
        compressed.push_back(final_block ? 0x01U : 0x00U);
        compressed.push_back(static_cast<unsigned char>(length & 0xffU));
        compressed.push_back(static_cast<unsigned char>((length >> 8U) & 0xffU));
        const std::uint16_t inverse = static_cast<std::uint16_t>(~length);
        compressed.push_back(static_cast<unsigned char>(inverse & 0xffU));
        compressed.push_back(static_cast<unsigned char>((inverse >> 8U) & 0xffU));
        compressed.insert(compressed.end(), raw.begin() + offset,
                          raw.begin() + offset + length);
        offset += length;
    }
    AppendBigEndian32(&compressed, Adler32(raw));

    std::vector<unsigned char> png;
    png.reserve(8U + 25U + 12U + compressed.size() + 12U);
    static constexpr unsigned char signature[] = {
        0x89U, 'P', 'N', 'G', '\r', '\n', 0x1aU, '\n'};
    png.insert(png.end(), std::begin(signature), std::end(signature));
    std::vector<unsigned char> header;
    header.reserve(13);
    AppendBigEndian32(&header, static_cast<std::uint32_t>(width));
    AppendBigEndian32(&header, static_cast<std::uint32_t>(height));
    header.push_back(8U);
    header.push_back(6U);
    header.push_back(0U);
    header.push_back(0U);
    header.push_back(0U);
    AppendChunk(&png, "IHDR", header);
    AppendChunk(&png, "IDAT", compressed);
    AppendChunk(&png, "IEND", {});
    return png;
}

int ReportSceneFailure(SceneError error, const Arguments &arguments) {
    const auto details = SceneErrorDetails(error);
    EmitError(details.first, details.second, &arguments);
    return error == SceneError::kX11 ? kExitCaptureFailure : kExitTargetMismatch;
}

}  // namespace

int main(int argc, char **argv) {
    (void)signal(SIGPIPE, SIG_IGN);
    const auto arguments = ParseArguments(argc, argv);
    if (!arguments.has_value()) {
        EmitError("invalid_arguments", "arguments");
        return kExitInvalid;
    }
    if (!SessionIsQualified()) {
        EmitError("unsupported_session", "session_preflight", &arguments.value());
        return kExitUnavailable;
    }
    if (DeadlineExpired(arguments->deadline_ns)) {
        EmitError("deadline_exceeded", "deadline_preflight", &arguments.value());
        return kExitDeadline;
    }
    auto process = OpenTrustedProcess(arguments->expected_pid);
    if (!process.has_value()) {
        EmitError("untrusted_process", "process_preflight", &arguments.value());
        return kExitTargetMismatch;
    }

    XSetErrorHandler(HandleXError);
    Display *display = XOpenDisplay(nullptr);
    if (display == nullptr) {
        EmitError("display_unavailable", "x11_preflight", &arguments.value());
        return kExitUnavailable;
    }
    const Atom pid_atom = XInternAtom(display, "_NET_WM_PID", True);
    if (pid_atom == None) {
        XCloseDisplay(display);
        EmitError("pid_property_unavailable", "target_preflight",
                  &arguments.value());
        return kExitTargetMismatch;
    }

    const SceneResult initial = InspectScene(display, pid_atom, arguments.value());
    if (initial.error != SceneError::kNone) {
        XCloseDisplay(display);
        return ReportSceneFailure(initial.error, arguments.value());
    }
    if (!ProcessIdentityUnchanged(arguments->expected_pid, process.value())) {
        XCloseDisplay(display);
        EmitError("process_changed", "process_pre_capture", &arguments.value());
        return kExitTargetMismatch;
    }
    if (DeadlineExpired(arguments->deadline_ns)) {
        XCloseDisplay(display);
        EmitError("deadline_exceeded", "deadline_pre_capture", &arguments.value());
        return kExitDeadline;
    }

    g_x_error = false;
    XGrabServer(display);
    if (!XOperationSucceeded(display)) {
        XUngrabServer(display);
        XSync(display, False);
        XCloseDisplay(display);
        EmitError("x11_error", "server_grab", &arguments.value());
        return kExitCaptureFailure;
    }
    bool server_grabbed = true;
    auto release_server = [&]() {
        if (server_grabbed) {
            XUngrabServer(display);
            XSync(display, False);
            server_grabbed = false;
        }
    };

    const SceneResult before = InspectScene(display, pid_atom, arguments.value());
    if (before.error != SceneError::kNone) {
        release_server();
        XCloseDisplay(display);
        return ReportSceneFailure(before.error, arguments.value());
    }
    if (!(before.snapshot == initial.snapshot) ||
        !ProcessIdentityUnchanged(arguments->expected_pid, process.value())) {
        release_server();
        XCloseDisplay(display);
        EmitError("scene_changed", "scene_pre_capture", &arguments.value());
        return kExitTargetMismatch;
    }
    if (DeadlineExpired(arguments->deadline_ns)) {
        release_server();
        XCloseDisplay(display);
        EmitError("deadline_exceeded", "deadline_pre_capture", &arguments.value());
        return kExitDeadline;
    }

    g_x_error = false;
    XImage *image = XGetImage(
        display, before.snapshot.root, arguments->x, arguments->y,
        static_cast<unsigned int>(arguments->width),
        static_cast<unsigned int>(arguments->height), AllPlanes, ZPixmap);
    const bool capture_ok = XOperationSucceeded(display);
    if (!capture_ok || image == nullptr) {
        if (image != nullptr) {
            XDestroyImage(image);
        }
        release_server();
        XCloseDisplay(display);
        EmitError("capture_failed", "xgetimage", &arguments.value());
        return kExitCaptureFailure;
    }

    const SceneResult after = InspectScene(display, pid_atom, arguments.value());
    const bool process_unchanged =
        ProcessIdentityUnchanged(arguments->expected_pid, process.value());
    const bool deadline_expired = DeadlineExpired(arguments->deadline_ns);
    release_server();
    if (after.error != SceneError::kNone) {
        XDestroyImage(image);
        XCloseDisplay(display);
        return ReportSceneFailure(after.error, arguments.value());
    }
    if (!(after.snapshot == before.snapshot) || !process_unchanged) {
        XDestroyImage(image);
        XCloseDisplay(display);
        EmitError("scene_changed", "scene_post_capture", &arguments.value());
        return kExitTargetMismatch;
    }
    if (deadline_expired) {
        XDestroyImage(image);
        XCloseDisplay(display);
        EmitError("deadline_exceeded", "deadline_post_capture", &arguments.value());
        return kExitDeadline;
    }
    XCloseDisplay(display);

    auto png = EncodePng(image, arguments->width, arguments->height);
    XDestroyImage(image);
    if (!png.has_value() || png->size() > kMaxPngBytes) {
        EmitError("png_encode_failed", "png_encode", &arguments.value());
        return kExitCaptureFailure;
    }
    if (DeadlineExpired(arguments->deadline_ns)) {
        EmitError("deadline_exceeded", "deadline_pre_output", &arguments.value());
        return kExitDeadline;
    }
    if (!WriteAll(STDOUT_FILENO, png->data(), png->size())) {
        EmitError("stdout_write_failed", "png_output", &arguments.value());
        return kExitOutputFailure;
    }
    if (!EmitSuccess(arguments.value(), after.snapshot, png->size())) {
        return kExitOutputFailure;
    }
    return 0;
}
