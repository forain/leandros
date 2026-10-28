/* leandros-snd-source — the PipeWire microphone source for LeandrOS.
 *
 * The capture half of /dev/pipewire (servers/pipewire, HD Audio backend):
 * an O_RDONLY|O_NONBLOCK open delivers 48 kHz S16LE stereo from the
 * microphone. poll(POLLIN) claims the reader and starts the capture stream;
 * it reports readiness once a 10 ms period is there. read() never returns 0:
 * EAGAIN when nothing is ready, EBUSY at the first poll/read if another open
 * already holds the microphone. The virtio-sound backend has no capture
 * (read fails ENODEV), in which case this program exits quietly and the
 * session simply has no source.
 *
 * This client publishes one Audio/Source node ("leandros-snd-source"). It is
 * NOT a driver: the sink drives the graph. The device is only open while
 * the node streams (opened on STREAMING, closed on PAUSED), so an idle
 * session leaves the capture DMA and the mic bias off. Samples are drained
 * into a small FIFO on the data loop when the fd is readable; each graph
 * cycle takes its quantum from the FIFO and pads with silence on a shortfall.
 * The FIFO is trimmed to a few quanta so clock drift between the codec and
 * the graph shows up as a dropped block, not as growing latency.
 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <spa/param/audio/format-utils.h>
#include <spa/utils/result.h>
#include <pipewire/pipewire.h>

#define DEV_PATH  "/dev/pipewire"
#define RATE      48000
#define CHANNELS  2
#define FRAME     (CHANNELS * 2)
#define QUANTUM   480u                  /* frames per cycle when nobody asks */
#define FIFO_FRAMES 8192u               /* power of two */
#define KEEP_FRAMES (QUANTUM * 6)       /* trim target when the FIFO runs ahead */

struct data {
	struct pw_main_loop *loop;
	struct pw_context *context;
	struct pw_loop *data_loop;
	struct pw_stream *stream;
	struct spa_hook stream_listener;
	struct spa_source *io;
	int fd;
	uint8_t fifo[FIFO_FRAMES * FRAME];
	uint32_t rd, wr;                    /* byte counters, wrap at 2^32 */
	uint64_t cycles, bytes, shortfalls, drops, last_report;
};

static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * SPA_NSEC_PER_SEC + ts.tv_nsec;
}

static uint32_t fifo_used(struct data *d) { return d->wr - d->rd; }

/* Data loop, fd readable: move everything the kernel has into the FIFO. */
static void on_io(void *userdata, int fd, uint32_t mask)
{
	struct data *d = userdata;
	uint8_t tmp[4096];

	if (mask & (SPA_IO_ERR | SPA_IO_HUP)) {
		fprintf(stderr, "leandros-snd-source: device error/hup, mask %#x\n", mask);
		return;
	}
	for (;;) {
		ssize_t r = read(d->fd, tmp, sizeof(tmp));
		if (r < 0 && errno == EINTR)
			continue;
		if (r <= 0)
			break;                  /* EAGAIN (or an error): wait for the next POLLIN */
		for (ssize_t i = 0; i < r; i++)
			d->fifo[(d->wr + i) % sizeof(d->fifo)] = tmp[i];
		d->wr += r;
		d->bytes += r;
		if (fifo_used(d) > sizeof(d->fifo) - 4096) {
			d->rd = d->wr - KEEP_FRAMES * FRAME;
			d->drops++;
		}
	}
}

static void on_process(void *userdata)
{
	struct data *d = userdata;
	struct pw_buffer *b;
	struct spa_data *sd;
	uint32_t frames, size, have;
	uint8_t *p;

	if ((b = pw_stream_dequeue_buffer(d->stream)) == NULL)
		return;
	sd = &b->buffer->datas[0];
	if (sd->data == NULL) {
		pw_stream_queue_buffer(d->stream, b);
		return;
	}
	frames = b->requested ? (uint32_t)b->requested : QUANTUM;
	frames = SPA_MIN(frames, sd->maxsize / FRAME);
	size = frames * FRAME;
	p = sd->data;

	/* Run-ahead beyond a few quanta is stale: keep the newest. */
	if (fifo_used(d) > KEEP_FRAMES * FRAME + size) {
		d->rd = d->wr - KEEP_FRAMES * FRAME;
		d->drops++;
	}
	have = SPA_MIN(fifo_used(d), size);
	have -= have % FRAME;
	for (uint32_t i = 0; i < have; i++)
		p[i] = d->fifo[(d->rd + i) % sizeof(d->fifo)];
	d->rd += have;
	if (have < size) {
		memset(p + have, 0, size - have);
		d->shortfalls++;
	}
	sd->chunk->offset = 0;
	sd->chunk->stride = FRAME;
	sd->chunk->size = size;
	pw_stream_queue_buffer(d->stream, b);
	d->cycles++;

	uint64_t t = now_ns();
	if (t - d->last_report > 10 * SPA_NSEC_PER_SEC) {
		fprintf(stderr, "leandros-snd-source: cycles=%llu bytes=%llu shortfalls=%llu drops=%llu\n",
			(unsigned long long)d->cycles, (unsigned long long)d->bytes,
			(unsigned long long)d->shortfalls, (unsigned long long)d->drops);
		d->last_report = t;
	}
}

/* Data loop: open the device and watch it. */
static int do_start(struct spa_loop *loop, bool async, uint32_t seq,
		const void *data, size_t size, void *user_data)
{
	struct data *d = user_data;

	if (d->fd >= 0)
		return 0;
	d->fd = open(DEV_PATH, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
	if (d->fd < 0) {
		fprintf(stderr, "leandros-snd-source: open %s: %s\n", DEV_PATH, strerror(errno));
		return 0;
	}
	d->rd = d->wr = 0;
	d->io = pw_loop_add_io(d->data_loop, d->fd, SPA_IO_IN | SPA_IO_ERR | SPA_IO_HUP,
			false, on_io, d);
	return 0;
}

static int do_stop(struct spa_loop *loop, bool async, uint32_t seq,
		const void *data, size_t size, void *user_data)
{
	struct data *d = user_data;

	if (d->io) {
		pw_loop_destroy_source(d->data_loop, d->io);
		d->io = NULL;
	}
	if (d->fd >= 0) {
		close(d->fd);               /* releases the mic: capture DMA and bias off */
		d->fd = -1;
	}
	return 0;
}

static void on_state_changed(void *userdata, enum pw_stream_state old,
		enum pw_stream_state state, const char *error)
{
	struct data *d = userdata;

	fprintf(stderr, "leandros-snd-source: %s -> %s%s%s\n",
		pw_stream_state_as_string(old), pw_stream_state_as_string(state),
		error ? ": " : "", error ? error : "");
	switch (state) {
	case PW_STREAM_STATE_STREAMING:
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
	.process = on_process,
};

static void do_quit(void *userdata, int signal_number)
{
	struct data *d = userdata;
	pw_main_loop_quit(d->loop);
}

/* Is there a capture device at all? 0 = yes, 1 = none (exit quietly). Waits
 * while another open holds the microphone. */
static int probe_capture(void)
{
	for (int waited = 0;; waited++) {
		uint8_t tmp[4];
		int fd = open(DEV_PATH, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
		if (fd < 0) {
			fprintf(stderr, "leandros-snd-source: open %s: %s\n", DEV_PATH, strerror(errno));
			return 1;
		}
		ssize_t r = read(fd, tmp, sizeof(tmp));
		int e = errno;
		close(fd);
		if (r >= 0 || e == EAGAIN)
			return 0;
		if (e == ENODEV) {
			fprintf(stderr, "leandros-snd-source: no capture device behind %s\n", DEV_PATH);
			return 1;
		}
		if (e != EBUSY) {
			fprintf(stderr, "leandros-snd-source: probe read: %s\n", strerror(e));
			return 1;
		}
		if (waited == 0)
			fprintf(stderr, "leandros-snd-source: microphone busy, waiting\n");
		sleep(1);
	}
}

int main(int argc, char *argv[])
{
	static struct data d = { .fd = -1 };
	const struct spa_pod *params[1];
	uint8_t buffer[1024];
	struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buffer, sizeof(buffer));
	struct spa_audio_info_raw info = {
		.format = SPA_AUDIO_FORMAT_S16_LE,
		.rate = RATE,
		.channels = CHANNELS,
		.position = { SPA_AUDIO_CHANNEL_FL, SPA_AUDIO_CHANNEL_FR },
	};
	int res;

	pw_init(&argc, &argv);
	if (probe_capture() != 0)
		return 0;

	d.loop = pw_main_loop_new(NULL);
	if (d.loop == NULL) {
		fprintf(stderr, "leandros-snd-source: pw_main_loop_new failed\n");
		return 1;
	}
	pw_loop_add_signal(pw_main_loop_get_loop(d.loop), SIGINT, do_quit, &d);
	pw_loop_add_signal(pw_main_loop_get_loop(d.loop), SIGTERM, do_quit, &d);

	struct pw_properties *props = pw_properties_new(
			PW_KEY_MEDIA_TYPE, "Audio",
			PW_KEY_MEDIA_CLASS, "Audio/Source",
			PW_KEY_NODE_NAME, "leandros-snd-source",
			PW_KEY_NODE_DESCRIPTION, "Microphone",
			PW_KEY_NODE_NICK, "Microphone",
			PW_KEY_DEVICE_ICON_NAME, "audio-input-microphone",
			PW_KEY_PRIORITY_SESSION, "1500",
			PW_KEY_NODE_LATENCY, "480/48000",
			PW_KEY_NODE_RATE, "1/48000",
			"audio.position", "[ FL FR ]",
			NULL);

	d.context = pw_context_new(pw_main_loop_get_loop(d.loop), NULL, 0);
	if (d.context == NULL) {
		fprintf(stderr, "leandros-snd-source: pw_context_new failed\n");
		return 1;
	}
	/* Same data loop as the stream's node (see leandros-snd-sink.c). */
	d.data_loop = pw_context_acquire_loop(d.context, &props->dict);

	struct pw_core *core = NULL;
	for (int tries = 0; core == NULL; tries++) {
		core = pw_context_connect(d.context, NULL, 0);
		if (core == NULL) {
			if (tries == 0)
				fprintf(stderr, "leandros-snd-source: waiting for pipewire: %s\n", strerror(errno));
			if (tries > 600)
				return 1;
			usleep(100 * 1000);
		}
	}

	d.stream = pw_stream_new(core, "leandros-snd-source", props);
	if (d.stream == NULL) {
		fprintf(stderr, "leandros-snd-source: pw_stream_new failed\n");
		return 1;
	}
	pw_stream_add_listener(d.stream, &d.stream_listener, &stream_events, &d);
	params[0] = spa_format_audio_raw_build(&b, SPA_PARAM_EnumFormat, &info);
	res = pw_stream_connect(d.stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
		PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS, params, 1);
	if (res < 0) {
		fprintf(stderr, "leandros-snd-source: connect: %s\n", spa_strerror(res));
		return 1;
	}
	fprintf(stderr, "leandros-snd-source: %s S16LE %u Hz %u ch\n", DEV_PATH, RATE, CHANNELS);

	pw_main_loop_run(d.loop);

	pw_stream_destroy(d.stream);
	pw_core_disconnect(core);
	pw_context_release_loop(d.context, d.data_loop);
	pw_context_destroy(d.context);
	pw_main_loop_destroy(d.loop);
	if (d.fd >= 0)
		close(d.fd);
	return 0;
}
