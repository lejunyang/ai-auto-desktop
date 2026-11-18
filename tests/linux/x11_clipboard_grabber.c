/*
 * A stand-in for the programs on a real desktop that watch CLIPBOARD and take
 * it themselves, for verify_assist_clipboard_x11.py:
 *
 *   x11_clipboard_grabber watch            logs every owner change
 *   x11_clipboard_grabber chansrv DELAY_MS [TIMES]
 *                                          on every owner change reads the new
 *                                          owner's TARGETS, then takes
 *                                          CLIPBOARD after DELAY_MS (what
 *                                          xrdp-chansrv does when the RDP
 *                                          client announces its clipboard),
 *                                          at most TIMES times (default: always)
 *   x11_clipboard_grabber klipper          on every owner change reads TARGETS
 *                                          and, when no text is offered, takes
 *                                          CLIPBOARD back with the last text it
 *                                          saw (Klipper's "prevent empty
 *                                          clipboard"); a marked password
 *                                          manager clipboard is left alone
 *
 * Every line it prints starts with "grabber:" and goes to stdout, flushed.
 * Build: cc -o grabber x11_clipboard_grabber.c -lX11 -lXfixes
 */
#include <X11/Xatom.h>
#include <X11/Xlib.h>
#include <X11/extensions/Xfixes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/time.h>

static Display *d;
static Window me;
static Atom clipboard, targets, utf8, prop, hint;
static char text[4096] = "grabbed";

static long ms(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return tv.tv_sec * 1000L + tv.tv_usec / 1000;
}

/* Waits for the SelectionNotify for our own request, answering requests in between. */
static int converted(Atom target, unsigned char **data, unsigned long *n, Atom *type);

static void serve(XSelectionRequestEvent *r) {
    XSelectionEvent n = {0};
    n.type = SelectionNotify;
    n.requestor = r->requestor;
    n.selection = r->selection;
    n.target = r->target;
    n.time = r->time;
    n.property = None;
    Atom property = r->property ? r->property : r->target;
    if (r->target == targets) {
        Atom list[] = {targets, utf8, XA_STRING};
        XChangeProperty(d, r->requestor, property, XA_ATOM, 32, PropModeReplace, (unsigned char *)list, 3);
        n.property = property;
    } else if (r->target == utf8 || r->target == XA_STRING) {
        XChangeProperty(d, r->requestor, property, utf8, 8, PropModeReplace, (unsigned char *)text, strlen(text));
        n.property = property;
    }
    printf("grabber: asked by 0x%lx for %s\n", r->requestor, XGetAtomName(d, r->target));
    fflush(stdout);
    XSendEvent(d, r->requestor, False, NoEventMask, (XEvent *)&n);
    XFlush(d);
}

static int converted(Atom target, unsigned char **data, unsigned long *n, Atom *type) {
    XDeleteProperty(d, me, prop);
    XConvertSelection(d, clipboard, target, prop, me, CurrentTime);
    XFlush(d);
    long until = ms() + 2000;
    while (ms() < until) {
        while (XPending(d)) {
            XEvent e;
            XNextEvent(d, &e);
            if (e.type == SelectionNotify && e.xselection.requestor == me) {
                if (e.xselection.property == None) return 0;
                int format;
                unsigned long after;
                XGetWindowProperty(d, me, prop, 0, 1 << 20, True, AnyPropertyType, type, &format, n, &after, data);
                return 1;
            }
            if (e.type == SelectionRequest) serve(&e.xselectionrequest);
        }
        struct timeval tv = {0, 5000};
        select(0, NULL, NULL, NULL, &tv);
    }
    return 0;
}

static void take(void) {
    XSetSelectionOwner(d, clipboard, me, CurrentTime);
    XFlush(d);
    printf("grabber: took CLIPBOARD\n");
    fflush(stdout);
}

int main(int argc, char **argv) {
    const char *mode = argc > 1 ? argv[1] : "watch";
    int delay = argc > 2 ? atoi(argv[2]) : 50;
    int times = argc > 3 ? atoi(argv[3]) : -1;
    d = XOpenDisplay(NULL);
    if (!d) return 1;
    me = XCreateSimpleWindow(d, DefaultRootWindow(d), 0, 0, 1, 1, 0, 0, 0);
    clipboard = XInternAtom(d, "CLIPBOARD", False);
    targets = XInternAtom(d, "TARGETS", False);
    utf8 = XInternAtom(d, "UTF8_STRING", False);
    prop = XInternAtom(d, "GRABBER", False);
    hint = XInternAtom(d, "x-kde-passwordManagerHint", False);
    int event_base, error_base;
    if (!XFixesQueryExtension(d, &event_base, &error_base)) return 2;
    XFixesSelectSelectionInput(d, me, clipboard, XFixesSetSelectionOwnerNotifyMask);
    printf("grabber: %s ready window 0x%lx\n", mode, me);
    fflush(stdout);
    long take_at = 0;
    for (;;) {
        while (XPending(d)) {
            XEvent e;
            XNextEvent(d, &e);
            if (e.type == event_base + XFixesSelectionNotify) {
                XFixesSelectionNotifyEvent *s = (XFixesSelectionNotifyEvent *)&e;
                printf("grabber: owner now 0x%lx\n", s->owner);
                fflush(stdout);
                if (s->owner == me || s->owner == None || !strcmp(mode, "watch")) continue;
                unsigned char *data = NULL;
                unsigned long n = 0;
                Atom type;
                int has_text = 0, secret = 0;
                if (converted(targets, &data, &n, &type) && data) {
                    Atom *list = (Atom *)data;
                    for (unsigned long i = 0; i < n; i++) {
                        char *name = XGetAtomName(d, list[i]);
                        printf("grabber:   offers %s\n", name);
                        if (list[i] == utf8 || list[i] == XA_STRING) has_text = 1;
                        if (list[i] == hint) secret = 1;
                        XFree(name);
                    }
                    fflush(stdout);
                    XFree(data);
                }
                if (!strcmp(mode, "chansrv")) {
                    if (times != 0) take_at = ms() + delay;
                    if (times > 0) times--;
                } else if (!strcmp(mode, "klipper")) {
                    if (secret) continue;
                    if (has_text && converted(utf8, &data, &n, &type) && data) {
                        snprintf(text, sizeof text, "%.*s", (int)n, data);
                        XFree(data);
                    } else if (!has_text) {
                        take();
                    }
                }
            } else if (e.type == SelectionRequest) {
                serve(&e.xselectionrequest);
            } else if (e.type == SelectionClear) {
                printf("grabber: lost CLIPBOARD\n");
                fflush(stdout);
            }
        }
        if (take_at && ms() >= take_at) {
            take_at = 0;
            take();
        }
        struct timeval tv = {0, 5000};
        select(0, NULL, NULL, NULL, &tv);
    }
}
