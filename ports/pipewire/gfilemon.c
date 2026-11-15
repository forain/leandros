/* gfilemon: drive a real GLib GFileMonitor (what COSMIC's file watchers and
 * GTK/GIO apps use) end to end over the kernel's inotify.  For each directory
 * argument: create, modify, rename and delete files and check the events that
 * arrive through the GMainLoop within a bound.
 * Output: "gfilemon: <dir>: <case>: PASS|FAIL [detail]"; exit 1 on any FAIL. */
#include <gio/gio.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>

#define BOUND_MS 5000
static GMainLoop *loop;
static unsigned seen;            /* bitmask of 1<<GFileMonitorEvent */
static GString *evlog;
static int nfail;

static void on_changed(GFileMonitor *m, GFile *f, GFile *o, GFileMonitorEvent ev, gpointer u)
{
    (void)m; (void)u;
    char *n = g_file_get_basename(f), *on = o ? g_file_get_basename(o) : NULL;
    seen |= 1u << ev;
    g_string_append_printf(evlog, " %d:%s%s%s", ev, n, on ? ">" : "", on ? on : "");
    g_free(n); g_free(on);
}

static gboolean tick(gpointer u) { g_main_loop_quit(loop); return G_SOURCE_REMOVE; }

/* Run the loop until every bit of `want` (any one of the alternatives in
 * `want_any`, if nonzero) is seen, or BOUND_MS passes. */
static int wait_for(unsigned want, unsigned want_any)
{
    gint64 end = g_get_monotonic_time() + BOUND_MS * 1000;
    GMainContext *c = g_main_context_default();
    while (g_get_monotonic_time() < end) {
        if ((seen & want) == want && (!want_any || (seen & want_any))) return 1;
        g_main_context_iteration(c, FALSE);
        g_usleep(2000);
    }
    return (seen & want) == want && (!want_any || (seen & want_any));
}

static void verdict(const char *dir, const char *name, int ok)
{
    printf("gfilemon: %s: %s: %s%s%s\n", dir, name, ok ? "PASS" : "FAIL",
           ok ? "" : " events:", ok ? "" : evlog->str);
    if (!ok) nfail++;
    fflush(stdout);
}

#define B(e) (1u << (e))

static void run_dir(const char *dir)
{
    char pa[512], pb[512], sub[512];
    snprintf(sub, sizeof sub, "%s/gfm.%d", dir, getpid());
    snprintf(pa, sizeof pa, "%s/a.txt", sub);
    snprintf(pb, sizeof pb, "%s/b.txt", sub);
    if (mkdir(sub, 0755)) { verdict(dir, "mkdir", 0); return; }

    GFile *gd = g_file_new_for_path(sub);
    GError *err = NULL;
    GFileMonitor *mon = g_file_monitor_directory(gd, G_FILE_MONITOR_WATCH_MOVES, NULL, &err);
    if (!mon) { printf("gfilemon: %s: monitor: FAIL %s\n", dir, err->message); nfail++; return; }
    g_signal_connect(mon, "changed", G_CALLBACK(on_changed), NULL);
    g_main_context_iteration(g_main_context_default(), FALSE);

    /* create + write + close */
    seen = 0; g_string_truncate(evlog, 0);
    int fd = open(pa, O_CREAT | O_WRONLY, 0644);
    verdict(dir, "create", fd >= 0 && wait_for(B(G_FILE_MONITOR_EVENT_CREATED), 0));
    if (fd >= 0) { write(fd, "hello", 5); close(fd); }
    verdict(dir, "modify", wait_for(B(G_FILE_MONITOR_EVENT_CHANGED), 0));
    verdict(dir, "changes_done", wait_for(B(G_FILE_MONITOR_EVENT_CHANGES_DONE_HINT), 0));

    /* rename inside the dir: RENAMED (WATCH_MOVES) */
    seen = 0; g_string_truncate(evlog, 0);
    rename(pa, pb);
    verdict(dir, "rename", wait_for(0, B(G_FILE_MONITOR_EVENT_RENAMED)));

    /* move out / in across the watched dir boundary */
    char outp[512]; snprintf(outp, sizeof outp, "%s/gfm-out.%d", dir, getpid());
    seen = 0; g_string_truncate(evlog, 0);
    rename(pb, outp);
    verdict(dir, "move_out", wait_for(0, B(G_FILE_MONITOR_EVENT_MOVED_OUT) | B(G_FILE_MONITOR_EVENT_DELETED)));
    seen = 0; g_string_truncate(evlog, 0);
    rename(outp, pa);
    verdict(dir, "move_in", wait_for(0, B(G_FILE_MONITOR_EVENT_MOVED_IN) | B(G_FILE_MONITOR_EVENT_CREATED)));

    /* delete */
    seen = 0; g_string_truncate(evlog, 0);
    unlink(pa);
    verdict(dir, "delete", wait_for(B(G_FILE_MONITOR_EVENT_DELETED), 0));

    /* no stray events after quiescence */
    seen = 0; g_string_truncate(evlog, 0);
    GMainContext *c = g_main_context_default();
    gint64 end = g_get_monotonic_time() + 500 * 1000;
    while (g_get_monotonic_time() < end) { g_main_context_iteration(c, FALSE); g_usleep(5000); }
    verdict(dir, "quiet", seen == 0);

    g_file_monitor_cancel(mon);
    g_object_unref(mon); g_object_unref(gd);
    rmdir(sub);
}

int main(int argc, char **argv)
{
    (void)tick;
    loop = g_main_loop_new(NULL, FALSE);
    evlog = g_string_new("");
    if (argc < 2) { fprintf(stderr, "usage: gfilemon DIR...\n"); return 2; }
    for (int i = 1; i < argc; i++) run_dir(argv[i]);
    printf("gfilemon: %s\n", nfail ? "FAILED" : "all passed");
    return nfail ? 1 : 0;
}
