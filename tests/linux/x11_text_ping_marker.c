#define _GNU_SOURCE
#include <X11/Xlib.h>
#include <dlfcn.h>
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

Status XSendEvent(Display *display, Window window, Bool propagate, long mask, XEvent *event) {
    Status (*send_event)(Display *, Window, Bool, long, XEvent *) = dlsym(RTLD_NEXT, "XSendEvent");
    const Status sent = send_event(display, window, propagate, mask, event);
    // Only loaded for the typing helper. Its client message to the target
    // without an event mask is the post-release _NET_WM_PING.
    if (sent && event->type == ClientMessage && mask == NoEventMask && event->xclient.window == window) {
        XFlush(display);
        const int marker = open(getenv("AAD_TEST_TEXT_PING"), O_CREAT | O_WRONLY, 0600);
        if (marker < 0) _exit(1);
        close(marker);
    }
    return sent;
}
