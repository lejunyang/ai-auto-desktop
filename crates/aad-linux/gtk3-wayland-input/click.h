/* Directed GTK button events. A pending pair owns its original GTK objects
 * and D-Bus message until the main loop has revalidated the release. */
typedef struct {
    GtkWidget *widget;
    GdkWindow *window;
    GdkDevice *pointer;
    double x, y;
} ClickHit;

static gboolean click_hit(const char *target, const char *top, int x, int y, ClickHit *hit) {
    GtkWidget *w = pointer_target(target, top);
    if (!GTK_IS_BUTTON(w) && !GTK_IS_DRAWING_AREA(w)) return FALSE;
    GtkWidget *leaf = widget_hit(w, x, y);
    /* The plain, windowless label of a stock button is its presentation.
     * A selectable label, markup link or embedded control owns its own hit. */
    if (leaf != w && !(GTK_IS_BUTTON(w) && GTK_IS_LABEL(leaf) &&
        gtk_bin_get_child(GTK_BIN(w)) == leaf && !gtk_widget_get_has_window(leaf) &&
        !gtk_label_get_selectable(GTK_LABEL(leaf)) && !gtk_label_get_use_markup(GTK_LABEL(leaf))))
        return FALSE;

    GdkWindow *events = GTK_IS_BUTTON(w) ? gtk_button_get_event_window(GTK_BUTTON(w))
                                         : gtk_widget_get_window(w);
    gpointer owner = NULL;
    GdkEventMask required = GDK_BUTTON_PRESS_MASK | GDK_BUTTON_RELEASE_MASK;
    if (GTK_IS_DRAWING_AREA(w) && (gtk_widget_get_events(w) & required) != required) return FALSE;
    if (!events || !gdk_window_is_viewable(events) ||
        (gdk_window_get_events(events) & required) != required) return FALSE;
    gdk_window_get_user_data(events, &owner);
    if (owner != w) return FALSE;
    GtkWidget *top_widget = gtk_widget_get_toplevel(w);
    GdkDisplay *display = gtk_widget_get_display(w);
    GdkSeat *seat = gdk_display_get_default_seat(display);
    GdkDevice *pointer = seat ? gdk_seat_get_pointer(seat) : NULL;
    if (!pointer || gdk_display_device_is_grabbed(display, pointer) ||
        gtk_window_group_get_current_device_grab(gtk_window_get_group(GTK_WINDOW(top_widget)), pointer))
        return FALSE;

    /* Include input-only GDK children that have no GtkWidget allocation.
     * Convert through each parent in window-local coordinates. */
    GdkWindow *branch = gtk_widget_get_window(top_widget);
    GdkModifierType held = 0;
    gdk_window_get_device_position(branch, pointer, NULL, NULL, &held);
    if (held & (GDK_BUTTON1_MASK | GDK_BUTTON2_MASK | GDK_BUTTON3_MASK |
                GDK_BUTTON4_MASK | GDK_BUTTON5_MASK)) return FALSE;
    double bx = x, by = y;
    for (int depth = 0; branch && depth < 64; depth++) {
        GdkWindow *next = NULL;
        double nx = 0, ny = 0;
        for (GList *it = gdk_window_peek_children(branch); it; it = it->next) {
            GdkWindow *child = it->data;
            if (!gdk_window_is_visible(child)) continue;
            double cx, cy;
            gdk_window_coords_from_parent(child, bx, by, &cx, &cy);
            if (cx < 0 || cy < 0 || cx >= gdk_window_get_width(child) ||
                cy >= gdk_window_get_height(child)) continue;
            if (next || gdk_window_is_shaped(child) || gdk_window_get_pass_through(child)) return FALSE;
            next = child;
            nx = cx;
            ny = cy;
        }
        if (!next) {
            if (branch != events) return FALSE;
            *hit = (ClickHit){w, events, pointer, bx, by};
            return TRUE;
        }
        branch = next;
        bx = nx;
        by = ny;
    }
    return FALSE;
}

static void send_button(const ClickHit *hit, GdkEventType type, guint32 time) {
    GdkEvent *event = gdk_event_new(type);
    event->button.window = g_object_ref(hit->window);
    event->button.send_event = TRUE;
    event->button.time = time;
    event->button.button = GDK_BUTTON_PRIMARY;
    event->button.state = type == GDK_BUTTON_RELEASE ? GDK_BUTTON1_MASK : 0;
    event->button.x = hit->x;
    event->button.y = hit->y;
    int rx, ry;
    gdk_window_get_root_coords(hit->window, (int)hit->x, (int)hit->y, &rx, &ry);
    event->button.x_root = rx;
    event->button.y_root = ry;
    gdk_event_set_device(event, hit->pointer);
    gdk_event_set_source_device(event, hit->pointer);
    gtk_widget_event(hit->widget, event);
    gdk_event_free(event);
}

typedef struct {
    ClickHit press;
    GtkWidget *toplevel;
    /* target and top are borrowed from the retained request. */
    const char *target, *top;
    int x, y;
    dbus_uint64_t until;
    DBusConnection *bus;
    DBusMessage *request;
    guint32 press_time;
    GPtrArray *gestures;
} PendingClick;

static gboolean owns_press(GtkGesture *gesture, const PendingClick *pair) {
    if (gtk_event_controller_get_widget(GTK_EVENT_CONTROLLER(gesture)) != pair->press.widget)
        return FALSE;
    const GdkEvent *last = gtk_gesture_get_last_event(gesture, NULL);
    return last && last->type == GDK_BUTTON_PRESS && last->button.send_event &&
           last->button.window == pair->press.window && last->button.time == pair->press_time &&
           last->button.button == GDK_BUTTON_PRIMARY && gdk_event_get_device(last) == pair->press.pointer &&
           last->button.x == pair->press.x && last->button.y == pair->press.y;
}

static gboolean remember_gesture(GSignalInvocationHint *hint, guint count,
                                 const GValue *args, gpointer data) {
    (void)hint;
    PendingClick *pair = data;
    GtkGesture *gesture = g_value_get_object(args);
    /* A NULL GdkEventSequence identifies the mouse, as opposed to a touch. */
    if (count == 2 && !g_value_get_boxed(args + 1) && owns_press(gesture, pair))
        g_ptr_array_add(pair->gestures, g_object_ref(gesture));
    return TRUE;
}

static void cancel_click(const PendingClick *pair) {
    const ClickHit *press = &pair->press;
    if (gtk_widget_in_destruction(press->widget) || !gtk_widget_get_realized(press->widget) ||
        gdk_window_is_destroyed(press->window) || widget_at_path(pair->target) != press->widget ||
        widget_at_path(pair->top) != pair->toplevel ||
        gtk_widget_get_toplevel(press->widget) != pair->toplevel) return;
    gpointer owner = NULL;
    gdk_window_get_user_data(press->window, &owner);
    if (owner != press->widget) return;
    /* Cancel the GTK gesture on the receiver that took the press, even if
     * it is now covered. This event has no hit point or release action. */
    GdkEvent *event = gdk_event_new(GDK_GRAB_BROKEN);
    event->grab_broken.window = g_object_ref(press->window);
    event->grab_broken.send_event = TRUE;
    event->grab_broken.keyboard = FALSE;
    event->grab_broken.implicit = TRUE;
    event->grab_broken.grab_window = NULL;
    gdk_event_set_device(event, press->pointer);
    gdk_event_set_source_device(event, press->pointer);
    gtk_widget_event(press->widget, event);
    gdk_event_free(event);
    /* GtkButton handles grab-broken itself, before its bubble gesture sees
     * it. Reset only controllers that still own our exact single press. */
    for (guint i = 0; i < pair->gestures->len; i++) {
        GtkGesture *gesture = g_ptr_array_index(pair->gestures, i);
        if (!owns_press(gesture, pair)) continue;
        GList *sequences = gtk_gesture_get_sequences(gesture);
        if (sequences && !sequences->next && !sequences->data)
            gtk_event_controller_reset(GTK_EVENT_CONTROLLER(gesture));
        g_list_free(sequences);
    }
}

static gboolean release_click(gpointer data) {
    PendingClick *pair = data;
    ClickHit hit;
    dbus_int32_t phases = 1;
    if (gtk_widget_get_mapped(pair->toplevel))
        gtk_container_check_resize(GTK_CONTAINER(pair->toplevel));
    if ((dbus_uint64_t)g_get_monotonic_time() * 1000 < pair->until &&
        click_hit(pair->target, pair->top, pair->x, pair->y, &hit) &&
        hit.widget == pair->press.widget && gtk_widget_get_toplevel(hit.widget) == pair->toplevel &&
        hit.window == pair->press.window && hit.pointer == pair->press.pointer &&
        hit.x == pair->press.x && hit.y == pair->press.y) {
        send_button(&hit, GDK_BUTTON_RELEASE, (guint32)(g_get_monotonic_time() / 1000));
        phases = 2;
    } else {
        cancel_click(pair);
    }
    delivering = FALSE;
    DBusMessage *reply = dbus_message_new_method_return(pair->request);
    dbus_message_append_args(reply, DBUS_TYPE_INT32, &phases, DBUS_TYPE_INVALID);
    dbus_connection_send(pair->bus, reply, NULL);
    dbus_message_unref(reply);
    dbus_message_unref(pair->request);
    dbus_connection_unref(pair->bus);
    g_object_unref(pair->press.widget);
    g_object_unref(pair->press.window);
    g_object_unref(pair->press.pointer);
    g_object_unref(pair->toplevel);
    g_ptr_array_unref(pair->gestures);
    g_free(pair);
    return G_SOURCE_REMOVE;
}

static gboolean start_click(DBusConnection *bus, DBusMessage *request,
                            const char *target, const char *top, int x, int y, dbus_uint64_t until) {
    ClickHit hit;
    if (delivering || !click_hit(target, top, x, y, &hit) ||
        (dbus_uint64_t)g_get_monotonic_time() * 1000 >= until) return FALSE;
    PendingClick *pair = g_new0(PendingClick, 1);
    *pair = (PendingClick){.press = hit, .toplevel = g_object_ref(gtk_widget_get_toplevel(hit.widget)),
        .target = target, .top = top, .x = x, .y = y, .until = until,
        .bus = dbus_connection_ref(bus), .request = dbus_message_ref(request),
        .press_time = (guint32)(g_get_monotonic_time() / 1000),
        .gestures = g_ptr_array_new_with_free_func(g_object_unref)};
    g_object_ref(hit.widget);
    g_object_ref(hit.window);
    g_object_ref(hit.pointer);
    delivering = TRUE;
    guint begin = g_signal_lookup("begin", GTK_TYPE_GESTURE);
    gulong hook = begin ? g_signal_add_emission_hook(begin, 0, remember_gesture, pair, NULL) : 0;
    send_button(&hit, GDK_BUTTON_PRESS, pair->press_time);
    if (hook) g_signal_remove_emission_hook(begin, hook);
    g_idle_add(release_click, pair);
    return TRUE;
}
