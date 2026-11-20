#include <gtk/gtk.h>
#include <stdio.h>

static GtkWidget *approval;

static void changed(GtkEditable *editable, gpointer title) {
    const char *text = gtk_entry_get_text(GTK_ENTRY(editable));
    printf("%s=%s\n", (char *) title, text);
    fflush(stdout);
    if (!g_strcmp0(title, "Invoice") && !g_strcmp0(text, "N"))
        gtk_widget_show_all(approval);
}

static void field(GtkWidget *window, const char *title, int x) {
    gtk_window_set_title(GTK_WINDOW(window), title);
    gtk_window_set_default_size(GTK_WINDOW(window), 220, 90);
    gtk_window_move(GTK_WINDOW(window), x, 100);
    GtkWidget *entry = gtk_entry_new();
    atk_object_set_name(gtk_widget_get_accessible(entry), "Reference");
    gtk_container_add(GTK_CONTAINER(GTK_IS_DIALOG(window)
        ? gtk_dialog_get_content_area(GTK_DIALOG(window)) : window), entry);
    g_signal_connect(entry, "changed", G_CALLBACK(changed), (gpointer)title);
    gtk_window_set_focus(GTK_WINDOW(window), entry);
}

int main(int argc, char **argv) {
    g_set_application_name("Mixed GTK recording fixture");
    gtk_init(&argc, &argv);
    GtkWidget *main_window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    GtkWidget *sibling = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    approval = gtk_dialog_new();
    gtk_window_set_transient_for(GTK_WINDOW(approval), GTK_WINDOW(main_window));
    field(main_window, "Invoice", 30);
    field(sibling, "Unrelated", 650);
    field(approval, "Approval", 330);
    gtk_widget_show_all(sibling);
    gtk_widget_show_all(main_window);
    puts("READY");
    fflush(stdout);
    gtk_main();
    return 0;
}
