#include <stdio.h>
#include <stdlib.h>
#include <X11/Xlib.h>
#include <X11/Xproto.h>
#include <X11/extensions/record.h>

static void receive(XPointer unused, XRecordInterceptData *record) {
    (void)unused;
    if (record->category == XRecordStartOfData) {
        puts("ready");
    } else if (record->category == XRecordFromServer && !record->client_swapped) {
        for (unsigned long i = 0; i + sizeof(xEvent) <= record->data_len * 4; i += sizeof(xEvent)) {
            const xEvent *event = (const xEvent *)(record->data + i);
            const int type = event->u.u.type & 0x7f;
            printf("%d %d %d %u %u\n", type, event->u.keyButtonPointer.rootX,
                   event->u.keyButtonPointer.rootY, event->u.u.detail,
                   event->u.keyButtonPointer.state);
            if (type == ButtonRelease && event->u.u.detail == 1) exit(0);
        }
    }
    XRecordFreeData(record);
}

int main(void) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    Display *control = XOpenDisplay(NULL);
    Display *data = XOpenDisplay(NULL);
    int major, minor;
    if (!control || !data || !XRecordQueryVersion(control, &major, &minor)) return 1;
    XRecordRange *range = XRecordAllocRange();
    if (!range) return 1;
    range->device_events.first = ButtonPress;
    range->device_events.last = MotionNotify;
    XRecordClientSpec clients = XRecordAllClients;
    XRecordContext context = XRecordCreateContext(control, 0, &clients, 1, &range, 1);
    XSync(control, False);
    XFree(range);
    return context && XRecordEnableContext(data, context, receive, NULL) ? 0 : 1;
}
