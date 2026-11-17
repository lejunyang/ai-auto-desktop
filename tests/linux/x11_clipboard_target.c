// A paste target for verify_assist_clipboard_x11.py: a window that, on
// Ctrl+V, asks the CLIPBOARD selection for its targets and then for each
// format a paste can carry, and prints one JSON line per paste:
// {"targets":[..],"text":..,"html":..,"rtf":..,"png":<bytes>,"files":..}.
// Build: cc -o x11_clipboard_target x11_clipboard_target.c -lX11
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static Display *display;
static Window window;
static Atom property;

static unsigned char *convert(Atom selection, Atom target, unsigned long *length, Atom *type) {
    XDeleteProperty(display, window, property);
    XConvertSelection(display, selection, target, property, window, CurrentTime);
    XFlush(display);
    for (int waited = 0; waited < 300; ++waited) {
        XEvent event;
        if (XCheckTypedWindowEvent(display, window, SelectionNotify, &event)) {
            if (event.xselection.property == None) return NULL;
            int format;
            unsigned long after;
            unsigned char *data = NULL;
            XGetWindowProperty(display, window, property, 0, 64 * 1024 * 1024, True, AnyPropertyType,
                               type, &format, length, &after, &data);
            if (format == 32) *length *= sizeof(long);
            return data;
        }
        usleep(10000);
    }
    return NULL;
}

static void json_string(const unsigned char *data, unsigned long length) {
    if (data == NULL) { printf("null"); return; }
    putchar('"');
    for (unsigned long i = 0; i < length; ++i) {
        unsigned char c = data[i];
        if (c == '"' || c == '\\') printf("\\%c", c);
        else if (c == '\n') printf("\\n");
        else if (c == '\r') printf("\\r");
        else if (c == '\t') printf("\\t");
        else if (c < 0x20) printf("\\u%04x", c);
        else putchar(c);
    }
    putchar('"');
}

int main(int argc, char **argv) {
    display = XOpenDisplay(NULL);
    if (!display) return 1;
    window = XCreateSimpleWindow(display, DefaultRootWindow(display), 100, 100, 400, 300, 1, 0, 0xffffff);
    XStoreName(display, window, argc > 1 ? argv[1] : "AAD clipboard target");
    long pid = getpid();
    XChangeProperty(display, window, XInternAtom(display, "_NET_WM_PID", False), XA_CARDINAL, 32,
                    PropModeReplace, (unsigned char *)&pid, 1);
    XSelectInput(display, window, KeyPressMask | ButtonPressMask | ExposureMask);
    XMapWindow(display, window);
    property = XInternAtom(display, "AAD_TARGET", False);
    Atom clipboard = XInternAtom(display, "CLIPBOARD", False);
    printf("{\"window\":%lu}\n", window);
    fflush(stdout);
    for (;;) {
        XEvent event;
        XNextEvent(display, &event);
        if (event.type == ButtonPress) {
            XSetInputFocus(display, window, RevertToParent, CurrentTime);
            continue;
        }
        if (event.type != KeyPress) continue;
        KeySym key = XLookupKeysym(&event.xkey, 0);
        if (key != XK_v || !(event.xkey.state & ControlMask)) continue;
        unsigned long length;
        Atom type;
        printf("{\"targets\":[");
        unsigned char *targets = convert(clipboard, XInternAtom(display, "TARGETS", False), &length, &type);
        for (unsigned long i = 0; targets && i < length / sizeof(long); ++i) {
            char *name = XGetAtomName(display, ((long *)targets)[i]);
            printf("%s\"%s\"", i ? "," : "", name ? name : "");
            if (name) XFree(name);
        }
        printf("]");
        const char *formats[][2] = {{"text", "UTF8_STRING"}, {"html", "text/html"}, {"rtf", "text/rtf"},
                                    {"files", "x-special/gnome-copied-files"}, {"uris", "text/uri-list"}};
        for (int f = 0; f < 5; ++f) {
            unsigned char *data = convert(clipboard, XInternAtom(display, formats[f][1], False), &length, &type);
            printf(",\"%s\":", formats[f][0]);
            json_string(data, data ? length : 0);
            if (data) XFree(data);
        }
        unsigned char *png = convert(clipboard, XInternAtom(display, "image/png", False), &length, &type);
        printf(",\"png\":%lu}\n", png ? length : 0);
        if (png) XFree(png);
        fflush(stdout);
    }
}
