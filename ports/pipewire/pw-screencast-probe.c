/* pw-screencast-probe — drive org.freedesktop.portal.ScreenCast end to end and
 * count the PipeWire video frames that arrive.
 *
 *   pw-screencast-probe [seconds=5] [out.ppm]
 *
 * CreateSession -> SelectSources(monitor) -> Start -> OpenPipeWireRemote, then a
 * pw_stream on the returned fd connected to the portal's node. Prints one
 * "frame" line per second and a final "RESULT frames=N ..." line; with a
 * second argument, the last frame is written as a binary PPM so the content
 * can be checked on the host. Start shows COSMIC's screen picker; pick a
 * screen and press Share.
 */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <gio/gio.h>
#include <gio/gunixfdlist.h>
#include <spa/param/video/format-utils.h>
#include <spa/debug/types.h>
#include <spa/param/video/type-info.h>
#include <pipewire/pipewire.h>

static GDBusConnection *bus;
static char *sender;
static GMainLoop *gloop;
static GVariant *response_results;
static guint32 response_code;
static int token_n;

static void on_response(GDBusConnection *c, const char *s, const char *path, const char *iface,
		const char *sig, GVariant *params, gpointer user)
{
	g_variant_get(params, "(u@a{sv})", &response_code, &response_results);
	g_main_loop_quit(gloop);
}

/* Call a portal method that answers through a Request object's Response. */
static int request(const char *method, GVariant *args_without_opts, GVariantBuilder *opts)
{
	char token[32], path[256];
	GError *err = NULL;
	snprintf(token, sizeof token, "probe%d_%d", getpid(), ++token_n);
	snprintf(path, sizeof path, "/org/freedesktop/portal/desktop/request/%s/%s", sender, token);
	g_variant_builder_add(opts, "{sv}", "handle_token", g_variant_new_string(token));
	guint sub = g_dbus_connection_signal_subscribe(bus, NULL,
		"org.freedesktop.portal.Request", "Response", path, NULL,
		G_DBUS_SIGNAL_FLAGS_NO_MATCH_RULE, on_response, NULL, NULL);
	/* the match rule explicitly (busd honours AddMatch) */
	char rule[512];
	snprintf(rule, sizeof rule, "type='signal',interface='org.freedesktop.portal.Request',"
		"member='Response',path='%s'", path);
	g_dbus_connection_call_sync(bus, "org.freedesktop.DBus", "/org/freedesktop/DBus",
		"org.freedesktop.DBus", "AddMatch", g_variant_new("(s)", rule), NULL, 0, -1, NULL, NULL);

	GVariantBuilder full;
	g_variant_builder_init(&full, G_VARIANT_TYPE_TUPLE);
	if (args_without_opts) {
		GVariantIter it;
		GVariant *child;
		g_variant_iter_init(&it, args_without_opts);
		while ((child = g_variant_iter_next_value(&it)))
			g_variant_builder_add_value(&full, child);
	}
	g_variant_builder_add_value(&full, g_variant_builder_end(opts));
	struct timespec t0, t1;
	clock_gettime(CLOCK_MONOTONIC, &t0);
	GVariant *r = g_dbus_connection_call_sync(bus, "org.freedesktop.portal.Desktop",
		"/org/freedesktop/portal/desktop", "org.freedesktop.portal.ScreenCast", method,
		g_variant_builder_end(&full), G_VARIANT_TYPE("(o)"), 0, 120000, NULL, &err);
	if (r == NULL) {
		fprintf(stderr, "probe: %s: %s\n", method, err->message);
		return -1;
	}
	g_variant_unref(r);
	response_results = NULL;
	g_main_loop_run(gloop);
	clock_gettime(CLOCK_MONOTONIC, &t1);
	g_dbus_connection_signal_unsubscribe(bus, sub);
	printf("probe: %s response=%u in %.0f ms\n", method, response_code,
		(t1.tv_sec - t0.tv_sec) * 1e3 + (t1.tv_nsec - t0.tv_nsec) / 1e6);
	fflush(stdout);
	return response_code == 0 ? 0 : -1;
}

struct vdata {
	struct pw_main_loop *loop;
	struct pw_stream *stream;
	struct spa_hook listener;
	struct spa_video_info_raw info;
	int have_format;
	uint64_t frames, empty, last_report;
	uint64_t t_first;
	uint32_t last_size;
	uint8_t *last;
	size_t last_len;
	uint32_t stride;
	const char *ppm;
};

static uint64_t mono_ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec * 1000000000ull + t.tv_nsec;
}

static void on_param_changed(void *u, uint32_t id, const struct spa_pod *param)
{
	struct vdata *d = u;
	if (param == NULL || id != SPA_PARAM_Format)
		return;
	if (spa_format_video_raw_parse(param, &d->info) < 0)
		return;
	d->have_format = 1;
	printf("probe: format %s %ux%u @ %u/%u\n",
		spa_debug_type_find_name(spa_type_video_format, d->info.format),
		d->info.size.width, d->info.size.height,
		d->info.framerate.num, d->info.framerate.denom);
	fflush(stdout);
}

static void on_process(void *u)
{
	struct vdata *d = u;
	struct pw_buffer *b = pw_stream_dequeue_buffer(d->stream);
	if (b == NULL)
		return;
	struct spa_data *sd = &b->buffer->datas[0];
	if (sd->data == NULL || sd->chunk->size == 0) {
		d->empty++;
	} else {
		if (d->frames == 0)
			d->t_first = mono_ns();
		d->frames++;
		d->last_size = sd->chunk->size;
		d->stride = sd->chunk->stride;
		if (d->ppm) {
			if (d->last_len < sd->chunk->size) {
				free(d->last);
				d->last = malloc(sd->chunk->size);
				d->last_len = sd->chunk->size;
			}
			memcpy(d->last, SPA_PTROFF(sd->data, sd->chunk->offset, void), sd->chunk->size);
		}
	}
	pw_stream_queue_buffer(d->stream, b);
	uint64_t now = mono_ns();
	if (now - d->last_report > 1000000000ull) {
		printf("probe: frames=%llu empty=%llu size=%u stride=%u type=%u\n",
			(unsigned long long)d->frames, (unsigned long long)d->empty,
			d->last_size, d->stride, sd->type);
		fflush(stdout);
		d->last_report = now;
	}
}

static void on_state(void *u, enum pw_stream_state o, enum pw_stream_state s, const char *e)
{
	printf("probe: stream %s -> %s%s%s\n", pw_stream_state_as_string(o),
		pw_stream_state_as_string(s), e ? ": " : "", e ? e : "");
	fflush(stdout);
}

static const struct pw_stream_events vevents = {
	PW_VERSION_STREAM_EVENTS,
	.state_changed = on_state,
	.param_changed = on_param_changed,
	.process = on_process,
};

static void on_quit_timer(void *u, uint64_t exp)
{
	pw_main_loop_quit(((struct vdata *)u)->loop);
}

int main(int argc, char *argv[])
{
	int secs = argc > 1 ? atoi(argv[1]) : 5;
	GError *err = NULL;
	struct vdata d = { 0 };
	d.ppm = argc > 2 ? argv[2] : NULL;

	bus = g_bus_get_sync(G_BUS_TYPE_SESSION, NULL, &err);
	if (!bus) { fprintf(stderr, "probe: bus: %s\n", err->message); return 1; }
	sender = g_strdup(g_dbus_connection_get_unique_name(bus) + 1);
	for (char *p = sender; *p; p++) if (*p == '.') *p = '_';
	gloop = g_main_loop_new(NULL, FALSE);

	GVariantBuilder o;
	g_variant_builder_init(&o, G_VARIANT_TYPE_VARDICT);
	g_variant_builder_add(&o, "{sv}", "session_handle_token", g_variant_new_string("probesession"));
	if (request("CreateSession", NULL, &o) < 0) return 2;
	const char *session = NULL;
	g_variant_lookup(response_results, "session_handle", "&s", &session);
	if (!session) { fprintf(stderr, "probe: no session_handle\n"); return 2; }
	session = g_strdup(session);
	printf("probe: session %s\n", session);

	g_variant_builder_init(&o, G_VARIANT_TYPE_VARDICT);
	g_variant_builder_add(&o, "{sv}", "types", g_variant_new_uint32(1));
	g_variant_builder_add(&o, "{sv}", "multiple", g_variant_new_boolean(FALSE));
	if (request("SelectSources", g_variant_new("(o)", session), &o) < 0) return 3;

	g_variant_builder_init(&o, G_VARIANT_TYPE_VARDICT);
	if (request("Start", g_variant_new("(os)", session, ""), &o) < 0) return 4;
	GVariant *streams = g_variant_lookup_value(response_results, "streams", G_VARIANT_TYPE("a(ua{sv})"));
	if (!streams || g_variant_n_children(streams) == 0) { fprintf(stderr, "probe: no streams\n"); return 4; }
	guint32 node;
	GVariant *sprops;
	g_variant_get_child(streams, 0, "(u@a{sv})", &node, &sprops);
	printf("probe: %zu stream(s), node %u\n", g_variant_n_children(streams), node);

	GUnixFDList *fds = NULL;
	GVariantBuilder e;
	g_variant_builder_init(&e, G_VARIANT_TYPE_VARDICT);
	GVariant *r = g_dbus_connection_call_with_unix_fd_list_sync(bus, "org.freedesktop.portal.Desktop",
		"/org/freedesktop/portal/desktop", "org.freedesktop.portal.ScreenCast", "OpenPipeWireRemote",
		g_variant_new("(oa{sv})", session, &e), G_VARIANT_TYPE("(h)"), 0, -1, NULL, &fds, NULL, &err);
	if (!r) { fprintf(stderr, "probe: OpenPipeWireRemote: %s\n", err->message); return 5; }
	gint32 idx;
	g_variant_get(r, "(h)", &idx);
	int fd = g_unix_fd_list_get(fds, idx, &err);
	printf("probe: pipewire fd %d\n", fd);

	pw_init(&argc, &argv);
	d.loop = pw_main_loop_new(NULL);
	struct pw_context *ctx = pw_context_new(pw_main_loop_get_loop(d.loop), NULL, 0);
	struct pw_core *core = pw_context_connect_fd(ctx, fd, NULL, 0);
	if (!core) { fprintf(stderr, "probe: connect_fd: %s\n", strerror(errno)); return 6; }
	d.stream = pw_stream_new(core, "pw-screencast-probe",
		pw_properties_new(PW_KEY_MEDIA_TYPE, "Video", PW_KEY_MEDIA_CATEGORY, "Capture",
			PW_KEY_MEDIA_ROLE, "Screen", NULL));
	pw_stream_add_listener(d.stream, &d.listener, &vevents, &d);

	uint8_t buf[1024];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof buf);
	const struct spa_pod *params[1];
	params[0] = spa_pod_builder_add_object(&b,
		SPA_TYPE_OBJECT_Format, SPA_PARAM_EnumFormat,
		SPA_FORMAT_mediaType, SPA_POD_Id(SPA_MEDIA_TYPE_video),
		SPA_FORMAT_mediaSubtype, SPA_POD_Id(SPA_MEDIA_SUBTYPE_raw),
		SPA_FORMAT_VIDEO_format, SPA_POD_CHOICE_ENUM_Id(5,
			SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_BGRx, SPA_VIDEO_FORMAT_RGBx,
			SPA_VIDEO_FORMAT_BGRA, SPA_VIDEO_FORMAT_RGBA),
		SPA_FORMAT_VIDEO_size, SPA_POD_CHOICE_RANGE_Rectangle(
			&SPA_RECTANGLE(1920, 1080), &SPA_RECTANGLE(1, 1), &SPA_RECTANGLE(8192, 8192)),
		SPA_FORMAT_VIDEO_framerate, SPA_POD_CHOICE_RANGE_Fraction(
			&SPA_FRACTION(30, 1), &SPA_FRACTION(0, 1), &SPA_FRACTION(240, 1)));
	if (pw_stream_connect(d.stream, PW_DIRECTION_INPUT, node,
			PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS, params, 1) < 0) {
		fprintf(stderr, "probe: stream connect failed\n");
		return 7;
	}
	struct spa_source *t = pw_loop_add_timer(pw_main_loop_get_loop(d.loop), on_quit_timer, &d);
	struct timespec v = { secs, 0 }, i = { 0, 0 };
	pw_loop_update_timer(pw_main_loop_get_loop(d.loop), t, &v, &i, false);
	uint64_t t0 = mono_ns();
	pw_main_loop_run(d.loop);
	double el = (mono_ns() - (d.t_first ? d.t_first : t0)) / 1e9;
	printf("RESULT frames=%llu empty=%llu fps=%.1f format=%ux%u size=%u\n",
		(unsigned long long)d.frames, (unsigned long long)d.empty,
		d.frames > 1 ? (d.frames - 1) / el : 0.0,
		d.info.size.width, d.info.size.height, d.last_size);
	if (d.ppm && d.last && d.have_format) {
		FILE *f = fopen(d.ppm, "wb");
		if (f) {
			uint32_t w = d.info.size.width, h = d.info.size.height;
			uint32_t stride = d.stride ? d.stride : w * 4;
			int bgr = d.info.format == SPA_VIDEO_FORMAT_BGRx || d.info.format == SPA_VIDEO_FORMAT_BGRA;
			fprintf(f, "P6\n%u %u\n255\n", w, h);
			for (uint32_t y = 0; y < h && (size_t)(y + 1) * stride <= d.last_len; y++)
				for (uint32_t x = 0; x < w; x++) {
					uint8_t *p = d.last + y * stride + x * 4;
					uint8_t px[3] = { bgr ? p[2] : p[0], p[1], bgr ? p[0] : p[2] };
					fwrite(px, 1, 3, f);
				}
			fclose(f);
			printf("probe: wrote %s\n", d.ppm);
		}
	}
	return d.frames > 0 ? 0 : 8;
}
