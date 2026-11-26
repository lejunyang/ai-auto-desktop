#define _GNU_SOURCE
#include <X11/Xlib.h>
#include <dlfcn.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static pid_t action_pid;

__attribute__((constructor)) static void remember_action_pid(void) {
    action_pid = getpid();
}

Display *XOpenDisplay(const char *name) {
    Display *(*open_display)(const char *) = dlsym(RTLD_NEXT, "XOpenDisplay");
    if (getpid() != action_pid) {
        // Delay the forked restorer past the action's cleanup grace, without
        // slowing the clipboard save or the Ctrl+V dispatch.
        FILE *marker = fopen(getenv("AAD_TEST_RESTORE_STARTED"), "w");
        if (marker == NULL) _exit(1);
        fclose(marker);
        const char *deadline = getenv("AAD_TEST_ACTION_DEADLINE_NS");
        if (deadline == NULL) {
            usleep(750000);
        } else {
            const unsigned long long wake = strtoull(deadline, NULL, 10) + 750000000;
            const struct timespec when = {
                .tv_sec = wake / 1000000000,
                .tv_nsec = wake % 1000000000,
            };
            while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &when, NULL) == EINTR) {}
        }
    }
    return open_display(name);
}
