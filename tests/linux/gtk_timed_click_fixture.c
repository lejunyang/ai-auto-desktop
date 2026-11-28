#include <gtk/gtk.h>
#include <gdk/gdkx.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

/* Adapted from the GTK click baseline (omem run r0mwxxemqfc29a6cb0e1).
 * Keep the original window, distances, widths and GDK event recording. */
static GtkWidget *window;
static GtkWidget *button;
static GtkWidget *count_label;
static GtkWidget *fixed;
static GtkWidget *focus_entry;
static GtkWidget *decoy;
static GtkWidget *other_window;
static const char *group;
static char *sample;
static unsigned activations;
static int origin_x;
static int origin_y;
static GtkAllocation bounds;

static long long now_us(void) {
    return (long long) g_get_monotonic_time();
}

static void clicked(GtkButton *widget, gpointer user_data) {
    (void) user_data;
    if (GTK_WIDGET(widget) == decoy) {
        printf("{\"kind\":\"decoy_clicked\",\"sample\":\"%s\"}\n", sample ? sample : "");
        return;
    }
    char text[64];
    snprintf(text, sizeof text, "Activations: %u", ++activations);
    gtk_label_set_text(GTK_LABEL(count_label), text);
    printf("{\"kind\":\"clicked\",\"group\":\"%s\",\"sample\":\"%s\",\"target\":\"%s\","
           "\"rx_us\":%lld,\"count\":%u}\n",
           group, sample ? sample : "", group, now_us(), activations);
}

static void dispatch(GdkEvent *event, gpointer user_data) {
    (void) user_data;
    const long long received = now_us();
    const GdkEventType type = gdk_event_get_event_type(event);
    const char *kind = NULL;
    switch (type) {
    case GDK_MOTION_NOTIFY: kind = "motion"; break;
    case GDK_BUTTON_PRESS: kind = "down"; break;
    case GDK_BUTTON_RELEASE: kind = "up"; break;
    case GDK_2BUTTON_PRESS: kind = "double_press"; break;
    case GDK_3BUTTON_PRESS: kind = "triple_press"; break;
    default: break;
    }
    if (kind && sample) {
        gdouble x, y;
        GdkModifierType state = 0;
        guint number = 0;
        if (gdk_event_get_root_coords(event, &x, &y)) {
            gdk_event_get_state(event, &state);
            gdk_event_get_button(event, &number);
            const gboolean hit =
                x >= origin_x + bounds.x && x < origin_x + bounds.x + bounds.width &&
                y >= origin_y + bounds.y && y < origin_y + bounds.y + bounds.height;
            printf("{\"kind\":\"%s\",\"group\":\"%s\",\"sample\":\"%s\",\"target\":\"%s\","
                   "\"rx_us\":%lld,\"x_ms\":%u,\"x\":%.3f,\"y\":%.3f,\"button\":%u,"
                   "\"state\":%u}\n",
                   kind, group, sample, hit ? group : "background", received,
                   gdk_event_get_time(event), x, y, number, (unsigned) state);
        }
    }
    gtk_main_do_event(event);
}

static gboolean ready(gpointer user_data) {
    (void) user_data;
    GdkWindow *win = gtk_widget_get_window(window);
    GdkWindow *button_win = gtk_button_get_event_window(GTK_BUTTON(button));
    gdk_window_get_origin(win, &origin_x, &origin_y);
    gtk_widget_get_allocation(button, &bounds);
    gdk_window_set_event_compression(win, FALSE);
    gdk_window_set_event_compression(button_win, FALSE);
    gdk_window_set_events(button_win, gdk_window_get_events(button_win) | GDK_POINTER_MOTION_MASK);
    printf("{\"kind\":\"ready\",\"group\":\"%s\",\"pid\":%ld,\"xid\":%lu,\"rx_us\":%lld,"
           "\"origin\":[%d,%d],\"box\":[%d,%d,%d,%d]}\n",
           group, (long) getpid(), gdk_x11_window_get_xid(win), now_us(), origin_x, origin_y,
           origin_x + bounds.x, origin_y + bounds.y, bounds.width, bounds.height);
    return G_SOURCE_REMOVE;
}

static gboolean command(GIOChannel *input, GIOCondition condition, gpointer user_data) {
    (void) user_data;
    if (!(condition & G_IO_IN)) {
        gtk_main_quit();
        return G_SOURCE_REMOVE;
    }
    char *line = NULL;
    if (g_io_channel_read_line(input, &line, NULL, NULL, NULL) != G_IO_STATUS_NORMAL) {
        gtk_main_quit();
        return G_SOURCE_REMOVE;
    }
    g_strchomp(line);
    if (g_str_has_prefix(line, "begin ") && !sample) {
        sample = g_strdup(line + 6);
        GdkDisplay *display = gtk_widget_get_display(window);
        GdkDevice *pointer = gdk_seat_get_pointer(gdk_display_get_default_seat(display));
        int x, y;
        gdk_device_get_position(pointer, NULL, &x, &y);
        printf("{\"kind\":\"begin\",\"group\":\"%s\",\"sample\":\"%s\",\"rx_us\":%lld,"
               "\"x\":%d,\"y\":%d,\"count\":%u}\n",
               group, sample, now_us(), x, y, activations);
    } else if (g_str_has_prefix(line, "end ") && g_strcmp0(line + 4, sample) == 0) {
        GdkWindow *win = gtk_widget_get_window(window);
        GdkDisplay *display = gdk_window_get_display(win);
        GdkDevice *pointer = gdk_seat_get_pointer(gdk_display_get_default_seat(display));
        GdkModifierType state = 0;
        gdk_device_get_state(pointer, win, NULL, &state);
        printf("{\"kind\":\"end\",\"group\":\"%s\",\"sample\":\"%s\",\"rx_us\":%lld,"
               "\"count\":%u,\"state\":%u}\n", group, sample, now_us(), activations, (unsigned) state);
        g_clear_pointer(&sample, g_free);
    } else if (g_strcmp0(line, "move") == 0 && !decoy) {
        decoy = gtk_button_new_with_label("Old spot");
        gtk_widget_set_size_request(decoy, bounds.width, bounds.height);
        gtk_fixed_put(GTK_FIXED(fixed), decoy, bounds.x, bounds.y);
        g_signal_connect(decoy, "clicked", G_CALLBACK(clicked), NULL);
        gtk_widget_show(decoy);
        gtk_fixed_move(GTK_FIXED(fixed), button, 520, bounds.y);
        g_timeout_add(80, ready, NULL);
    } else if (g_strcmp0(line, "focus-other-control") == 0) {
        gtk_widget_show(focus_entry);
        gtk_widget_grab_focus(focus_entry);
    } else if (g_strcmp0(line, "cover-target") == 0 && decoy) {
        gtk_fixed_move(GTK_FIXED(fixed), decoy, bounds.x, bounds.y);
    } else if (g_strcmp0(line, "uncover-target") == 0 && decoy) {
        gtk_widget_hide(decoy);
    } else if (g_strcmp0(line, "focus-other-window") == 0) {
        other_window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
        gtk_window_set_title(GTK_WINDOW(other_window), "aad Click Focus Sink");
        gtk_window_set_default_size(GTK_WINDOW(other_window), 260, 100);
        gtk_window_move(GTK_WINDOW(other_window), 45, 520);
        gtk_container_add(GTK_CONTAINER(other_window), gtk_entry_new());
        gtk_widget_show_all(other_window);
        gtk_window_present(GTK_WINDOW(other_window));
    } else if (g_strcmp0(line, "quit") == 0) {
        g_free(line);
        gtk_main_quit();
        return G_SOURCE_REMOVE;
    } else {
        fprintf(stderr, "invalid fixture command: %s\n", line);
        exit(2);
    }
    g_free(line);
    return G_SOURCE_CONTINUE;
}

int main(int argc, char **argv) {
    if (argc != 4) {
        return 2;
    }
    group = argv[1];
    const int center_x = atoi(argv[2]);
    const int width = atoi(argv[3]);

    setvbuf(stdout, NULL, _IOLBF, 0);
    gtk_init(NULL, NULL);
    window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "aad Click Baseline");
    gtk_window_set_default_size(GTK_WINDOW(window), 960, 360);
    gtk_window_set_decorated(GTK_WINDOW(window), FALSE);
    gtk_window_move(GTK_WINDOW(window), 40, 40);
    gtk_widget_add_events(window, GDK_POINTER_MOTION_MASK | GDK_BUTTON_PRESS_MASK |
                                  GDK_BUTTON_RELEASE_MASK);
    g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);
    fixed = gtk_fixed_new();
    gtk_container_add(GTK_CONTAINER(window), fixed);
    count_label = gtk_label_new("Activations: 0");
    gtk_fixed_put(GTK_FIXED(fixed), count_label, 20, 20);
    focus_entry = gtk_entry_new();
    gtk_widget_set_no_show_all(focus_entry, TRUE);
    gtk_fixed_put(GTK_FIXED(fixed), focus_entry, 20, 270);
    button = gtk_button_new_with_label(group);
    gtk_widget_set_size_request(button, width, 60);
    gtk_fixed_put(GTK_FIXED(fixed), button, center_x - width / 2, 150);
    g_signal_connect(button, "clicked", G_CALLBACK(clicked), NULL);
    gdk_event_handler_set(dispatch, NULL, NULL);
    gtk_widget_show_all(window);
    g_timeout_add(200, ready, NULL);

    GIOChannel *input = g_io_channel_unix_new(STDIN_FILENO);
    g_io_add_watch(input, G_IO_IN | G_IO_HUP | G_IO_ERR, command, NULL);
    gtk_main();
    g_io_channel_unref(input);
    return 0;
}
