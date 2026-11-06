// Isolated X11 protocol regression: no focus, input, or clipboard ownership is
// changed. Run through verify-x11-paste.ps1, which supplies a private Xvfb.
#define main aad_xtest_helper_main
#include "../../plugins/linux_atspi/x11_xtest_helper.cpp"
#undef main

#include <stdexcept>

namespace {
void Require(bool value, const char *message) {
    if (!value) throw std::runtime_error(message);
}

std::vector<unsigned long> ReadAtoms(Display *display, Window window, Atom property) {
    Atom type = None;
    int format = 0;
    unsigned long count = 0, remaining = 0;
    unsigned char *data = nullptr;
    Require(XGetWindowProperty(display, window, property, 0, 32, True, XA_ATOM,
                               &type, &format, &count, &remaining, &data) == Success,
            "cannot read TARGETS response");
    Require(type == XA_ATOM && format == 32 && remaining == 0, "bad TARGETS response");
    auto *values = reinterpret_cast<unsigned long *>(data);
    std::vector<unsigned long> result(values, values + count);
    XFree(data);
    return result;
}

std::optional<std::string> ReadBytes(Display *display, Window window, Atom property) {
    Atom type = None;
    int format = 0;
    unsigned long count = 0, remaining = 0;
    unsigned char *data = nullptr;
    Require(XGetWindowProperty(display, window, property, 0, 1024, True, AnyPropertyType,
                               &type, &format, &count, &remaining, &data) == Success,
            "cannot read conversion response");
    std::optional<std::string> result;
    if (type != None) {
        Require(format == 8 && remaining == 0, "bad text response");
        result = std::string(reinterpret_cast<char *>(data), count);
    }
    if (data) XFree(data);
    return result;
}
}

int main() {
    try {
        XSetErrorHandler(HandleXError);
        Display *owner = XOpenDisplay(nullptr);
        Display *target = XOpenDisplay(nullptr);
        Display *observer = XOpenDisplay(nullptr);
        Require(owner && target && observer, "private X server unavailable");
        const Selection selection = OpenSelection(owner);
        const Atom pid_atom = XInternAtom(owner, "_NET_WM_PID", False);
        const Atom property = XInternAtom(owner, "AAD_PASTE_PROTOCOL_TEST", False);
        const unsigned long pid = static_cast<unsigned long>(getpid());
        const Window target_window = XCreateSimpleWindow(target, DefaultRootWindow(target),
                                                         0, 0, 1, 1, 0, 0, 0);
        const Window target_requestor = XCreateSimpleWindow(target, DefaultRootWindow(target),
                                                            0, 0, 1, 1, 0, 0, 0);
        const Window other_requestor = XCreateSimpleWindow(observer, DefaultRootWindow(observer),
                                                           0, 0, 1, 1, 0, 0, 0);
        XChangeProperty(target, target_window, pid_atom, XA_CARDINAL, 32, PropModeReplace,
                        reinterpret_cast<const unsigned char *>(&pid), 1);
        XSync(target, False);
        XSync(observer, False);
        const auto mask = ResourceMask(owner, selection.window);
        Require(mask.has_value(), "XID resource mask unavailable");
        Require(SameClient(owner, target_requestor, target_window, mask, pid, pid_atom),
                "anonymous target requestor must retain XID ownership validation");
        Require(!SameClient(owner, other_requestor, target_window, mask, pid, pid_atom),
                "unrelated client must fail ownership validation");
        const std::string payload = "AAD private paste \xe4\xb8\xad\xe6\x96\x87";
        const std::vector<Item> offer = {
            {selection.utf8, selection.utf8, 8, payload},
            {selection.plain_utf8, selection.plain_utf8, 8, payload},
            {selection.text, selection.utf8, 8, payload},
            {selection.password_hint, selection.password_hint, 8, "secret"},
        };
        auto request = [&](Window requestor, Atom atom, std::optional<XID> client_mask) {
            XSelectionRequestEvent event{};
            event.owner = selection.window;
            event.requestor = requestor;
            event.selection = selection.clipboard;
            event.target = atom;
            event.property = property;
            return ServePaste(selection, event, offer, target_window, client_mask, pid, pid_atom);
        };

        Require(request(other_requestor, selection.targets, mask) == PasteReply::Metadata,
                "clipboard observers need a nonempty metadata offer");
        const auto public_targets = ReadAtoms(observer, other_requestor, property);
        Require(public_targets == std::vector<unsigned long>{selection.targets, selection.password_hint},
                "unrelated clients must see only the content-free privacy marker");
        Require(request(other_requestor, selection.password_hint, mask) == PasteReply::Metadata,
                "privacy marker must not count as text consumption");
        Require(ReadBytes(observer, other_requestor, property) == "secret", "privacy marker mismatch");
        Require(request(other_requestor, selection.utf8, mask) == PasteReply::Rejected,
                "unrelated text request must be rejected");
        Require(!ReadBytes(observer, other_requestor, property), "text leaked to unrelated client");

        Require(request(target_requestor, selection.targets, mask) == PasteReply::Metadata,
                "target needs supported text formats");
        const auto target_targets = ReadAtoms(target, target_requestor, property);
        Require(Contains(target_targets, selection.utf8) && Contains(target_targets, selection.password_hint),
                "target format list incomplete");
        Require(request(target_requestor, selection.utf8, mask) == PasteReply::Text,
                "verified target text request must be served");
        Require(ReadBytes(target, target_requestor, property) == payload, "target text differs");
        Require(request(target_requestor, selection.utf8, std::nullopt) == PasteReply::Rejected,
                "anonymous target must fail closed without XID validation");
        Require(!ReadBytes(target, target_requestor, property), "unverified request received text");

        const unsigned long wrong_pid = pid + 1;
        XChangeProperty(target, target_requestor, pid_atom, XA_CARDINAL, 32, PropModeReplace,
                        reinterpret_cast<const unsigned char *>(&wrong_pid), 1);
        XSync(target, False);
        Require(request(target_requestor, selection.utf8, mask) == PasteReply::Rejected,
                "explicit PID mismatch must override same-client fallback");
        Require(!ReadBytes(target, target_requestor, property), "PID mismatch received text");
        XCloseDisplay(observer);
        XCloseDisplay(target);
        XCloseDisplay(owner);
        std::cout << "PASS: public metadata, private text, anonymous XID ownership, PID mismatch, missing mask\n";
        return 0;
    } catch (const std::exception &error) {
        std::cerr << "FAIL: " << error.what() << '\n';
        return 1;
    }
}
