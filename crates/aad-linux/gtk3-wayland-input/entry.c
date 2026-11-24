/* Opt-in GTK 3 module. Resolve only the active window and its focused entry
 * on GTK's thread. No seat injection or screen coordinates are involved. */
#include <atspi/atspi.h>
#include <dbus/dbus.h>
#include <gdk/gdkwayland.h>
#include <gtk/gtk.h>

#define IFACE "org.aiautodesktop.Gtk3WidgetInput1"
#define PATH "/org/aiautodesktop/Gtk3WidgetInput1"

/* The ATK bridge exports this query on supported versions. A weak import
 * lets a module built for another bridge omit its endpoint safely. It only
 * reads the registration of an already exposed object. */
extern guint spi_register_object_to_ref(GObject *) __attribute__((weak));

static char *registered_path(GtkWidget *widget) {
    guint id = spi_register_object_to_ref(G_OBJECT(gtk_widget_get_accessible(widget)));
    return id ? g_strdup_printf("/org/a11y/atspi/accessible/%u", id) : NULL;
}

#include "scroll.h"

static GtkWidget *focused_entry(void) {
    GList *windows = gtk_window_list_toplevels();
    GtkWidget *entry = NULL;
    for (GList *item = windows; item; item = item->next) {
        GtkWindow *window = GTK_WINDOW(item->data);
        if (!gtk_window_is_active(window)) continue;
        GtkWidget *focus = gtk_window_get_focus(window);
        if (entry || !GTK_IS_ENTRY(focus) || !gtk_widget_has_focus(focus) ||
            gtk_widget_get_toplevel(focus) != GTK_WIDGET(window) ||
            !gtk_widget_get_mapped(focus) || !gtk_widget_is_sensitive(focus) ||
            !gtk_entry_get_visibility(GTK_ENTRY(focus)) ||
            !gtk_editable_get_editable(GTK_EDITABLE(focus)) ||
            gtk_grab_get_current() != NULL) {
            entry = NULL;
            break;
        }
        GtkInputPurpose purpose = gtk_entry_get_input_purpose(GTK_ENTRY(focus));
        if (purpose == GTK_INPUT_PURPOSE_PASSWORD || purpose == GTK_INPUT_PURPOSE_PIN ||
            atk_object_get_role(gtk_widget_get_accessible(focus)) == ATK_ROLE_PASSWORD_TEXT) {
            entry = NULL;
            break;
        }
        entry = focus;
    }
    g_list_free(windows);
    return entry;
}

static gboolean matches(GtkWidget *entry, const char *target, const char *top) {
    if (!entry || entry != focused_entry()) return FALSE;
    g_autofree char *actual = registered_path(entry);
    g_autofree char *window = registered_path(gtk_widget_get_toplevel(entry));
    return actual && window && !strcmp(target, actual) && !strcmp(top, window);
}

static int send_key(const char *target, const char *top, const char *key,
                    const char *modifier, dbus_uint64_t until) {
    static gboolean delivering;
    GtkWidget *entry = focused_entry();
    if (delivering || !matches(entry, target, top)) return 0;
    GdkModifierType state = 0;
    if (!strcmp(modifier, "ctrl")) state = GDK_CONTROL_MASK;
    else if (*modifier) return 0;
    guint keyval;
    if (strlen(key) == 1 && (g_ascii_islower(*key) || g_ascii_isdigit(*key))) keyval = *key;
    else if (!strcmp(key, "backspace")) keyval = GDK_KEY_BackSpace;
    else if (!strcmp(key, "left")) keyval = GDK_KEY_Left;
    else if (!strcmp(key, "right")) keyval = GDK_KEY_Right;
    else return 0;
    /* Limit ctrl chords to select-all. Other shortcuts can affect the
     * clipboard or transfer focus outside the observed entry. */
    if (state && keyval != GDK_KEY_a) return 0;
    GdkDisplay *display = gtk_widget_get_display(entry);
    GdkDevice *keyboard = gdk_seat_get_keyboard(gdk_display_get_default_seat(display));
    GdkKeymap *keymap = gdk_keymap_get_for_display(display);
    g_autofree GdkKeymapKey *keys = NULL;
    int n = 0;
    if (!keyboard || !gdk_keymap_get_entries_for_keyval(keymap, keyval, &keys, &n)) return 0;
    int index = 0;
    while (index < n && (keys[index].group != 0 || keys[index].level != 0)) index++;
    if (index == n || keys[index].keycode > G_MAXUINT16) return 0;

    g_object_ref(entry);
    delivering = TRUE;
    int phases = 0;
    for (; phases < 2; phases++) {
        if ((dbus_uint64_t)g_get_monotonic_time() * 1000 >= until ||
            !matches(entry, target, top)) break;
        GdkEvent *event = gdk_event_new(phases ? GDK_KEY_RELEASE : GDK_KEY_PRESS);
        event->key.window = g_object_ref(gtk_widget_get_window(entry));
        event->key.send_event = TRUE;
        event->key.time = (guint32)(g_get_monotonic_time() / 1000);
        event->key.state = state;
        event->key.keyval = keyval;
        event->key.hardware_keycode = keys[index].keycode;
        gdk_event_set_device(event, keyboard);
        gtk_widget_event(entry, event);
        gdk_event_free(event);
    }
    delivering = FALSE;
    g_object_unref(entry);
    return phases;
}

static DBusHandlerResult message(DBusConnection *bus, DBusMessage *request, void *data) {
    (void)data;
    DBusMessage *reply;
    if (dbus_message_is_method_call(request, IFACE, "FocusedEntry") &&
        dbus_message_has_signature(request, "")) {
        GtkWidget *entry = focused_entry();
        g_autofree char *target = entry ? registered_path(entry) : NULL;
        g_autofree char *window = entry ? registered_path(gtk_widget_get_toplevel(entry)) : NULL;
        const char *a = target ? target : "", *b = window ? window : "";
        reply = dbus_message_new_method_return(request);
        dbus_message_append_args(reply, DBUS_TYPE_STRING, &a, DBUS_TYPE_STRING, &b, DBUS_TYPE_INVALID);
    } else if (dbus_message_is_method_call(request, IFACE, "WheelBounds") &&
               dbus_message_has_signature(request, "ss")) {
        const char *target, *top;
        dbus_message_get_args(request, NULL, DBUS_TYPE_STRING, &target, DBUS_TYPE_STRING, &top, DBUS_TYPE_INVALID);
        GtkWidget *w = wheel_target(target, top);
        int x = 0, y = 0, width = 0, height = 0;
        if (w && gtk_widget_translate_coordinates(w, gtk_widget_get_toplevel(w), 0, 0, &x, &y)) {
            width = gtk_widget_get_allocated_width(w);
            height = gtk_widget_get_allocated_height(w);
        }
        reply = dbus_message_new_method_return(request);
        dbus_message_append_args(reply, DBUS_TYPE_INT32, &x, DBUS_TYPE_INT32, &y,
                                 DBUS_TYPE_INT32, &width, DBUS_TYPE_INT32, &height, DBUS_TYPE_INVALID);
    } else if (dbus_message_is_method_call(request, IFACE, "WheelHit") &&
               dbus_message_has_signature(request, "ssii")) {
        const char *target, *top;
        int x, y;
        dbus_message_get_args(request, NULL, DBUS_TYPE_STRING, &target, DBUS_TYPE_STRING, &top,
                              DBUS_TYPE_INT32, &x, DBUS_TYPE_INT32, &y, DBUS_TYPE_INVALID);
        dbus_bool_t hit = wheel_hit(target, top, x, y) != NULL;
        reply = dbus_message_new_method_return(request);
        dbus_message_append_args(reply, DBUS_TYPE_BOOLEAN, &hit, DBUS_TYPE_INVALID);
    } else if (dbus_message_is_method_call(request, IFACE, "Wheel") &&
               dbus_message_has_signature(request, "ssiiiit")) {
        static gboolean scrolling;
        const char *target, *top;
        int x, y, direction, notches;
        dbus_uint64_t until;
        dbus_message_get_args(request, NULL, DBUS_TYPE_STRING, &target, DBUS_TYPE_STRING, &top,
                              DBUS_TYPE_INT32, &x, DBUS_TYPE_INT32, &y, DBUS_TYPE_INT32, &direction,
                              DBUS_TYPE_INT32, &notches, DBUS_TYPE_UINT64, &until, DBUS_TYPE_INVALID);
        int result = 0;
        if (!scrolling) {
            scrolling = TRUE;
            result = wheel(target, top, x, y, direction, notches, until);
            scrolling = FALSE;
        }
        reply = dbus_message_new_method_return(request);
        dbus_message_append_args(reply, DBUS_TYPE_INT32, &result, DBUS_TYPE_INVALID);
    } else if (dbus_message_is_method_call(request, IFACE, "Key") &&
               dbus_message_has_signature(request, "sssst")) {
        const char *target, *window, *key, *modifier;
        dbus_uint64_t until;
        if (!dbus_message_get_args(request, NULL, DBUS_TYPE_STRING, &target, DBUS_TYPE_STRING, &window,
                                   DBUS_TYPE_STRING, &key, DBUS_TYPE_STRING, &modifier,
                                   DBUS_TYPE_UINT64, &until, DBUS_TYPE_INVALID))
            return DBUS_HANDLER_RESULT_NOT_YET_HANDLED;
        dbus_int32_t phases = send_key(target, window, key, modifier, until);
        reply = dbus_message_new_method_return(request);
        dbus_message_append_args(reply, DBUS_TYPE_INT32, &phases, DBUS_TYPE_INVALID);
    } else return DBUS_HANDLER_RESULT_NOT_YET_HANDLED;
    dbus_connection_send(bus, reply, NULL);
    dbus_message_unref(reply);
    return DBUS_HANDLER_RESULT_HANDLED;
}

static gboolean register_endpoint(gpointer data) {
    (void)data;
    static const DBusObjectPathVTable vtable = {.message_function = message};
    if (GDK_IS_WAYLAND_DISPLAY(gdk_display_get_default()) && spi_register_object_to_ref) {
        DBusConnection *bus = atspi_get_a11y_bus();
        if (bus) dbus_connection_register_object_path(bus, PATH, &vtable, NULL);
    }
    return G_SOURCE_REMOVE;
}

G_MODULE_EXPORT void gtk_module_init(gint *argc, gchar ***argv) {
    (void)argc;
    (void)argv;
    g_idle_add(register_endpoint, NULL);
}
