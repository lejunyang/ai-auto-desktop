/* Window-local wheel input for GTK ranges and drawing areas. */
extern GObject *spi_global_register_path_to_object(const char *) __attribute__((weak));
extern void *spi_global_register __attribute__((weak));

static GtkWidget *widget_at_path(const char *path) {
    const char *prefix = "/org/a11y/atspi/accessible/";
    if (!spi_global_register_path_to_object || !&spi_global_register || !spi_global_register ||
        !g_str_has_prefix(path, prefix)) return NULL;
    const char *digits = path + strlen(prefix);
    guint64 id;
    if (!g_ascii_string_to_unsigned(digits, 10, 1, G_MAXINT, &id, NULL)) return NULL;
    GObject *object = spi_global_register_path_to_object(path);
    if (!GTK_IS_ACCESSIBLE(object)) return NULL;
    GtkWidget *w = gtk_accessible_get_widget(GTK_ACCESSIBLE(object));
    if (!w || gtk_widget_get_accessible(w) != ATK_OBJECT(object)) return NULL;
    g_autofree char *canonical = registered_path(w);
    return g_strcmp0(path, canonical) == 0 ? w : NULL;
}

static GtkWidget *pointer_target(const char *target, const char *top) {
    GtkWidget *w = widget_at_path(target);
    GtkWidget *window = widget_at_path(top);
    if (!w || !GTK_IS_WINDOW(window) ||
        gtk_widget_get_toplevel(w) != window || !gtk_window_is_active(GTK_WINDOW(window)) ||
        !gtk_widget_get_mapped(w) || !gtk_widget_is_sensitive(w) || gtk_grab_get_current())
        return NULL;
    return w;
}

static GtkWidget *wheel_target(const char *target, const char *top) {
    GtkWidget *w = pointer_target(target, top);
    return GTK_IS_RANGE(w) || GTK_IS_DRAWING_AREA(w) ? w : NULL;
}

typedef struct {
    GtkWidget *window;
    int x, y, count;
    GtkWidget *child;
} Pick;

static void pick_child(GtkWidget *child, gpointer data) {
    Pick *pick = data;
    int x, y;
    if (gtk_widget_get_mapped(child) &&
        gtk_widget_translate_coordinates(pick->window, child, pick->x, pick->y, &x, &y) &&
        x >= 0 && y >= 0 && x < gtk_widget_get_allocated_width(child) &&
        y < gtk_widget_get_allocated_height(child)) {
        pick->count++;
        pick->child = child;
    }
}

static GtkWidget *widget_hit(GtkWidget *w, int x, int y) {
    if (!w) return NULL;
    GtkWidget *window = gtk_widget_get_toplevel(w);
    GtkWidget *branch = window;
    if (x < 0 || y < 0 || x >= gtk_widget_get_allocated_width(window) ||
        y >= gtk_widget_get_allocated_height(window)) return NULL;
    for (int depth = 0; GTK_IS_CONTAINER(branch); depth++) {
        if (depth >= 64) return NULL;
        Pick pick = {window, x, y, 0, NULL};
        gtk_container_forall(GTK_CONTAINER(branch), pick_child, &pick);
        if (pick.count == 0) break;
        /* Stacking and pass-through semantics vary by container. An overlap
         * cannot establish an exclusive hit on this branch. */
        if (pick.count != 1) return NULL;
        branch = pick.child;
    }
    return branch;
}

static GtkWidget *wheel_hit(const char *target, const char *top, int x, int y) {
    GtkWidget *w = wheel_target(target, top);
    return w && widget_hit(w, x, y) == w ? w : NULL;
}

static int wheel(const char *target, const char *top, int x, int y, int direction,
                 int notches, dbus_uint64_t until) {
    static const GdkScrollDirection directions[] = {
        GDK_SCROLL_UP, GDK_SCROLL_DOWN, GDK_SCROLL_LEFT, GDK_SCROLL_RIGHT
    };
    if (direction < 4 || direction > 7 || notches < 1 || notches > 30) return 0;
    int sent = 0;
    while (sent < notches && (dbus_uint64_t)g_get_monotonic_time() * 1000 < until) {
        GtkWidget *w = wheel_hit(target, top, x, y);
        if (!w) break;
        GdkWindow *event_window = gtk_widget_get_window(w);
        gpointer owner = NULL;
        gdk_window_get_user_data(event_window, &owner);
        int wx, wy;
        if (!GTK_IS_WIDGET(owner) || gtk_widget_get_window(GTK_WIDGET(owner)) != event_window ||
            !gtk_widget_translate_coordinates(gtk_widget_get_toplevel(w), GTK_WIDGET(owner),
                                              x, y, &wx, &wy)) break;
        GdkDevice *device = gdk_seat_get_pointer(gdk_display_get_default_seat(gtk_widget_get_display(w)));
        if (!device) break;
        GdkEvent *event = gdk_event_new(GDK_SCROLL);
        event->scroll.window = g_object_ref(event_window);
        event->scroll.send_event = TRUE;
        event->scroll.time = (guint32)(g_get_monotonic_time() / 1000);
        event->scroll.x = wx;
        event->scroll.y = wy;
        event->scroll.direction = directions[direction - 4];
        gdk_event_set_device(event, device);
        g_object_ref(w);
        gtk_widget_event(w, event);
        g_object_unref(w);
        gdk_event_free(event);
        sent++;
    }
    return sent == notches ? 2 : sent ? 1 : 0;
}
