#define _GNU_SOURCE
#include <X11/Xlib.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static pid_t action_pid;
static Atom pending_selection;
static Window pending_owner;

__attribute__((constructor)) static void remember_action_pid(void) {
    action_pid = getpid();
}

static void mark(const char *path) {
    FILE *marker = fopen(path, "w");
    if (marker == NULL) _exit(1);
    fclose(marker);
}

static void delay_until(const char *deadline, unsigned long long delay_ns) {
    struct timespec when;
    if (deadline == NULL) {
        when = (struct timespec){.tv_sec = 0, .tv_nsec = delay_ns};
        while (clock_nanosleep(CLOCK_MONOTONIC, 0, &when, &when) == EINTR) {}
    } else {
        const unsigned long long wake = strtoull(deadline, NULL, 10) + delay_ns;
        when = (struct timespec){.tv_sec = wake / 1000000000, .tv_nsec = wake % 1000000000};
        while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &when, NULL) == EINTR) {}
    }
}

Display *XOpenDisplay(const char *name) {
    Display *(*open_display)(const char *) = dlsym(RTLD_NEXT, "XOpenDisplay");
    if (getpid() != action_pid) {
        // Delay the forked restorer past the action's cleanup grace, without
        // slowing the clipboard save or the Ctrl+V dispatch.
        mark(getenv("AAD_TEST_RESTORE_STARTED"));
        delay_until(getenv("AAD_TEST_RESTORE_AFTER_NS"), 750000000);
    }
    return open_display(name);
}

int XSetSelectionOwner(Display *display, Atom selection, Window owner, Time time) {
    int (*set_owner)(Display *, Atom, Window, Time) = dlsym(RTLD_NEXT, "XSetSelectionOwner");
    if (getpid() == action_pid && owner != None && getenv("AAD_TEST_OWNER_REPLY")) {
        pending_selection = selection;
        pending_owner = owner;
    }
    return set_owner(display, selection, owner, time);
}

Window XGetSelectionOwner(Display *display, Atom selection) {
    Window (*get_owner)(Display *, Atom) = dlsym(RTLD_NEXT, "XGetSelectionOwner");
    Window owner = get_owner(display, selection);
    if (getpid() == action_pid && pending_owner != None && selection == pending_selection) {
        if (owner != pending_owner) _exit(1);
        pending_owner = None;
        mark(getenv("AAD_TEST_OWNER_REPLY"));
        // X11 has accepted the new owner. Hold the confirmation before the
        // paste helper sees it, across either its deadline or an MCP cancel.
        const char *deadline = getenv("AAD_TEST_OWNER_REPLY_AFTER_NS");
        delay_until(deadline, deadline ? 100000000 : 450000000);
    }
    return owner;
}
