/* leandros-snd-sink — the PipeWire audio sink for LeandrOS's virtio-sound.
 *
 * LeandrOS has no ALSA ABI (no /dev/snd). The kernel's audio path is the
 * in-kernel server behind /dev/pipewire (servers/pipewire): write() takes
 * interleaved S16LE PCM into a spool that feeds the virtio-sound TX queue,
 * ioctl 0x101 sets {u32 rate, u8 channels}, ioctl 0x102 reports how many bytes
 * are queued ahead of the DAC (spool + device ring).
 *
 * This client publishes one Audio/Sink node ("node.name" leandros_output) to
 * the PipeWire graph and is that graph's DRIVER, timed exactly like
 * spa/plugins/alsa's timer-based scheduling: a timer fires once per quantum,
 * the queued delay is read back, a DLL (spa/utils/dll.h) steers the timer
 * rate so the delay stays at TARGET frames, and the cycle's mixed output is
 * written with a plain write(). The device clock (QEMU's audio backend) is
 * therefore the graph clock, writes never block (the spool never fills), and
 * the kernel's silence top-up only ever runs on a real underrun.
 *
 * Volume and mute are not ours: the stream's own audioconvert applies the
 * node's Props (channelVolumes/mute), which is what cosmic-settings-daemon
 * and wpctl set.
 */
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#include <spa/param/audio/format-utils.h>
#include <spa/param/props.h>
#include <spa/utils/dll.h>
#include <spa/utils/result.h>
#include <pipewire/pipewire.h>

#define DEV_PATH     "/dev/pipewire"
#define IOC_SET_PARAMS 0x101
#define IOC_GET_DELAY  0x102

#define RATE      48000
#define CHANNELS  2
#define FRAME     (CHANNELS * 2)
#define QUANTUM   1024u                 /* frames per cycle when nobody asks */
#define TARGET    (QUANTUM * 4)         /* frames kept queued ahead of the DAC */
#define MAX_ERROR 256.0

struct data {
	struct pw_main_loop *loop;
	struct pw_context *context;
	struct pw_loop *data_loop;
	struct pw_stream *stream;
	struct spa_hook stream_listener;
	struct spa_source *timer;
	struct spa_io_position *position;
	struct spa_dll dll;
	double corr;
	uint64_t next_time;
	int fd;
	int driving;
	/* statistics, printed every ~10 s */
	uint64_t cycles, bytes, underruns, last_report;
	int64_t min_delay, max_delay;
};

static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * SPA_NSEC_PER_SEC + ts.tv_nsec;
}

static int64_t queued_frames(struct data *d)
{
	uint32_t bytes = 0;
	if (ioctl(d->fd, IOC_GET_DELAY, &bytes) < 0)
		return -1;
	return bytes / FRAME;
}

static void set_timeout(struct data *d, uint64_t t)
{
	struct timespec value = { t / SPA_NSEC_PER_SEC, t % SPA_NSEC_PER_SEC };
	struct timespec interval = { 0, 0 };
	pw_loop_update_timer(d->data_loop, d->timer, &value, &interval, true);
}

/* Data-loop timer: one graph cycle. */
static void on_timeout(void *userdata, uint64_t expirations)
{
	struct data *d = userdata;
	struct spa_io_position *pos = d->position;
	uint64_t duration = QUANTUM, current = d->next_time;
	uint32_t rate = RATE;
	int64_t delay;
	double err;

	if (pos) {
		duration = pos->clock.target_duration ? pos->clock.target_duration : QUANTUM;
		rate = pos->clock.target_rate.denom ? pos->clock.target_rate.denom : RATE;
	}

	delay = queued_frames(d);
	if (delay < 0)
		delay = TARGET;
	if (delay < d->min_delay) d->min_delay = delay;
	if (delay > d->max_delay) d->max_delay = delay;
	if (delay < (int64_t)duration / 2)
		d->underruns++;

	if (delay > (int64_t)(3 * TARGET)) {
		/* Far ahead (e.g. the timer was late and caught up): skip this
		 * cycle's audio by re-arming one quantum later without producing. */
		d->next_time = current + duration * SPA_NSEC_PER_SEC / rate;
		set_timeout(d, d->next_time);
		return;
	}

	err = (double)(delay - (int64_t)TARGET);
	err = SPA_CLAMP(err, -MAX_ERROR, MAX_ERROR);
	d->corr = spa_dll_update(&d->dll, err);
	d->next_time = current + (uint64_t)(duration / d->corr * 1e9 / rate);

	if (pos) {
		pos->clock.nsec = current;
		pos->clock.rate = pos->clock.target_rate;
		pos->clock.position += pos->clock.duration;
		pos->clock.duration = duration;
		pos->clock.delay = delay;
		pos->clock.rate_diff = d->corr;
		pos->clock.next_nsec = d->next_time;
	}
	set_timeout(d, d->next_time);
	d->cycles++;
	pw_stream_trigger_process(d->stream);
}

static void on_process(void *userdata)
{
	struct data *d = userdata;
	struct pw_buffer *b;
	struct spa_data *sd;
	uint32_t offs, size;
	uint8_t *p;

	if ((b = pw_stream_dequeue_buffer(d->stream)) == NULL)
		return;
	sd = &b->buffer->datas[0];
	if (sd->data != NULL && sd->chunk != NULL) {
		offs = SPA_MIN(sd->chunk->offset, sd->maxsize);
		size = SPA_MIN(sd->chunk->size, sd->maxsize - offs);
		size -= size % FRAME;
		p = SPA_PTROFF(sd->data, offs, uint8_t);
		while (size > 0) {
			ssize_t w = write(d->fd, p, size);
			if (w < 0 && errno == EINTR)
				continue;
			if (w <= 0)
				break;
			p += w;
			size -= w;
			d->bytes += w;
		}
	}
	pw_stream_queue_buffer(d->stream, b);

	uint64_t t = now_ns();
	if (t - d->last_report > 10 * SPA_NSEC_PER_SEC) {
		fprintf(stderr, "leandros-snd-sink: cycles=%llu bytes=%llu corr=%.6f "
			"delay[min=%lld max=%lld target=%u] underruns=%llu\n",
			(unsigned long long)d->cycles, (unsigned long long)d->bytes, d->corr,
			(long long)d->min_delay, (long long)d->max_delay, TARGET,
			(unsigned long long)d->underruns);
		d->last_report = t;
		d->min_delay = INT64_MAX;
		d->max_delay = 0;
	}
}

static void on_io_changed(void *userdata, uint32_t id, void *area, uint32_t size)
{
	struct data *d = userdata;
	if (id == SPA_IO_Position)
		d->position = area;
}

static int do_start(struct spa_loop *loop, bool async, uint32_t seq,
		const void *data, size_t size, void *user_data)
{
	struct data *d = user_data;
	spa_dll_init(&d->dll);
	spa_dll_set_bw(&d->dll, SPA_DLL_BW_MIN, QUANTUM, RATE);
	d->corr = 1.0;
	d->next_time = now_ns();
	set_timeout(d, d->next_time);
	return 0;
}

static int do_stop(struct spa_loop *loop, bool async, uint32_t seq,
		const void *data, size_t size, void *user_data)
{
	struct data *d = user_data;
	set_timeout(d, 0);
	return 0;
}

static void on_state_changed(void *userdata, enum pw_stream_state old,
		enum pw_stream_state state, const char *error)
{
	struct data *d = userdata;
	fprintf(stderr, "leandros-snd-sink: %s -> %s%s%s\n",
		pw_stream_state_as_string(old), pw_stream_state_as_string(state),
		error ? ": " : "", error ? error : "");
	switch (state) {
	case PW_STREAM_STATE_STREAMING:
		d->driving = pw_stream_is_driving(d->stream);
		if (d->driving)
			pw_loop_invoke(d->data_loop, do_start, 0, NULL, 0, true, d);
		break;
	case PW_STREAM_STATE_PAUSED:
		pw_loop_invoke(d->data_loop, do_stop, 0, NULL, 0, true, d);
		break;
	case PW_STREAM_STATE_ERROR:
	case PW_STREAM_STATE_UNCONNECTED:
		pw_main_loop_quit(d->loop);
		break;
	default:
		break;
	}
}

static const struct pw_stream_events stream_events = {
	PW_VERSION_STREAM_EVENTS,
	.state_changed = on_state_changed,
	.io_changed = on_io_changed,
	.process = on_process,
};

static void do_quit(void *userdata, int signal_number)
{
	struct data *d = userdata;
	pw_main_loop_quit(d->loop);
}

int main(int argc, char *argv[])
{
	struct data d = { .fd = -1, .min_delay = INT64_MAX, .corr = 1.0 };
	const struct spa_pod *params[1];
	uint8_t buffer[1024];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
	struct spa_audio_info_raw info = {
		.format = SPA_AUDIO_FORMAT_S16_LE,
		.rate = RATE,
		.channels = CHANNELS,
		.position = { SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR },
	};
	uint8_t setp[8] = { 0 };
	uint32_t rate = RATE;
	int res;

	pw_init(&argc, &argv);

	/* The device is single-writer (servers/pipewire): while a console player
	 * (aplay, MAME) holds it, open() is EBUSY. Wait for it like a PipeWire ALSA
	 * node waits for a busy card, instead of leaving the session silent. */
	for (int waited = 0;; waited++) {
		if ((d.fd = open(DEV_PATH, O_WRONLY | O_CLOEXEC)) >= 0)
			break;
		if (errno != EBUSY) {
			fprintf(stderr, "leandros-snd-sink: open %s: %s\n", DEV_PATH, strerror(errno));
			return 1;
		}
		if (waited == 0)
			fprintf(stderr, "leandros-snd-sink: %s busy (held by another player), waiting\n",
				DEV_PATH);
		usleep(500 * 1000);
	}
	memcpy(setp, &rate, 4);
	setp[4] = CHANNELS;
	if (ioctl(d.fd, IOC_SET_PARAMS, setp) < 0) {
		fprintf(stderr, "leandros-snd-sink: SET_PARAMS: %s\n", strerror(errno));
		return 1;
	}
	if (queued_frames(&d) < 0) {
		fprintf(stderr, "leandros-snd-sink: kernel lacks GET_DELAY (ioctl 0x102): %s\n",
			strerror(errno));
		return 1;
	}

	d.loop = pw_main_loop_new(NULL);
	if (d.loop == NULL) {
		fprintf(stderr, "leandros-snd-sink: pw_main_loop_new failed\n");
		return 1;
	}
	pw_loop_add_signal(pw_main_loop_get_loop(d.loop), SIGINT, do_quit, &d);
	pw_loop_add_signal(pw_main_loop_get_loop(d.loop), SIGTERM, do_quit, &d);

	struct pw_properties *props = pw_properties_new(
			PW_KEY_MEDIA_TYPE, "Audio",
			PW_KEY_MEDIA_CLASS, "Audio/Sink",
			PW_KEY_NODE_NAME, "leandros_output",
			PW_KEY_NODE_DESCRIPTION, "Virtio Sound Output",
			PW_KEY_NODE_NICK, "Virtio Sound",
			PW_KEY_DEVICE_ICON_NAME, "audio-card",
			PW_KEY_PRIORITY_DRIVER, "1500",
			PW_KEY_PRIORITY_SESSION, "1500",
			PW_KEY_NODE_LATENCY, "1024/48000",
			PW_KEY_NODE_RATE, "1/48000",
			"node.pause-on-idle", "false",
			"audio.position", "[ FL FR ]",
			NULL);

	d.context = pw_context_new(pw_main_loop_get_loop(d.loop), NULL, 0);
	if (d.context == NULL) {
		fprintf(stderr, "leandros-snd-sink: pw_context_new failed\n");
		return 1;
	}
	/* The stream's node runs on the context's data.rt loop; acquire the same
	 * one (same props, like module-pipe-tunnel) so the timer and process()
	 * share a thread. */
	d.data_loop = pw_context_acquire_loop(d.context, &props->dict);
	d.timer = pw_loop_add_timer(d.data_loop, on_timeout, &d);

	struct pw_core *core = NULL;
	for (int tries = 0; core == NULL; tries++) {
		core = pw_context_connect(d.context, NULL, 0);
		if (core == NULL) {
			if (tries == 0)
				fprintf(stderr, "leandros-snd-sink: waiting for pipewire: %s\n", strerror(errno));
			if (tries > 600)
				return 1;
			usleep(100 * 1000);
		}
	}

	d.stream = pw_stream_new(core, "leandros-snd-sink", props);
	if (d.stream == NULL) {
		fprintf(stderr, "leandros-snd-sink: pw_stream_new failed\n");
		return 1;
	}
	pw_stream_add_listener(d.stream, &d.stream_listener, &stream_events, &d);

	params[0] = spa_format_audio_raw_build(&b, SPA_PARAM_EnumFormat, &info);

	res = pw_stream_connect(d.stream, PW_DIRECTION_INPUT, PW_ID_ANY,
		PW_STREAM_FLAG_DRIVER | PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS,
		params, 1);
	if (res < 0) {
		fprintf(stderr, "leandros-snd-sink: connect: %s\n", spa_strerror(res));
		return 1;
	}
	fprintf(stderr, "leandros-snd-sink: %s S16LE %u Hz %u ch, target %u frames\n",
		DEV_PATH, RATE, CHANNELS, TARGET);

	pw_main_loop_run(d.loop);

	pw_stream_destroy(d.stream);
	pw_core_disconnect(core);
	pw_context_release_loop(d.context, d.data_loop);
	pw_context_destroy(d.context);
	pw_main_loop_destroy(d.loop);
	close(d.fd);
	pw_deinit();
	return 0;
}
