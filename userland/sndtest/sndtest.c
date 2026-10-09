/* sndtest — exercise LeandrOS's playback device through /dev/pipewire, the
 * same path PipeWire's leandros-snd-sink uses (write() of S16LE PCM,
 * ioctl 0x101 SET_PARAMS {u32 rate, u8 channels}, ioctl 0x102 GET_DELAY),
 * plus ioctl 0x103 SND_GET_INFO (driver state as text: backend, controller
 * and codec ids, chosen output pins, DMA counters).
 *
 *   sndtest info                    print the driver state
 *   sndtest tone [hz] [sec] [rate]  sine on both channels (default 440 2 48000)
 *   sndtest lr [sec]                left only (440 Hz), then right only (660 Hz)
 *   sndtest all                     info, tone, lr, info, summary (default)
 *
 * Plain C (static musl via zig, like nettest); the summary lines start with
 * "sndtest:" so a boot log can be grepped.
 */
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define DEV "/dev/pipewire"
#define IOC_SET_PARAMS 0x101
#define IOC_GET_DELAY 0x102
#define IOC_GET_INFO 0x103

static char info_buf[4096];

static double now_s(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

static const char *get_info(void)
{
	int fd = open(DEV, O_RDONLY);
	if (fd < 0) {
		snprintf(info_buf, sizeof info_buf, "(open %s: %s)\n", DEV, strerror(errno));
		return info_buf;
	}
	memset(info_buf, 0, sizeof info_buf);
	if (ioctl(fd, IOC_GET_INFO, info_buf) < 0)
		snprintf(info_buf, sizeof info_buf, "(SND_GET_INFO: %s; kernel predates lane hda)\n", strerror(errno));
	close(fd);
	return info_buf;
}

/* Value of "key <number>" in the info text, or -1. */
static long long info_num(const char *text, const char *key)
{
	const char *p = strstr(text, key);
	if (!p)
		return -1;
	p += strlen(key);
	while (*p == ' ')
		p++;
	return strtoll(p, NULL, 10);
}

static int open_out(unsigned rate, unsigned ch)
{
	int fd = open(DEV, O_WRONLY);
	if (fd < 0) {
		printf("sndtest: open %s for writing: %s%s\n", DEV, strerror(errno),
		       errno == EBUSY ? " (held by another player, e.g. the PipeWire session)" : "");
		return -1;
	}
	unsigned char p[8] = {0};
	memcpy(p, &rate, 4);
	p[4] = (unsigned char)ch;
	if (ioctl(fd, IOC_SET_PARAMS, p) < 0) {
		printf("sndtest: SET_PARAMS %u Hz x%u: %s\n", rate, ch, strerror(errno));
		close(fd);
		return -1;
	}
	return fd;
}

static unsigned delay_bytes(int fd)
{
	uint32_t d = 0;
	if (ioctl(fd, IOC_GET_DELAY, &d) < 0)
		return 0;
	return d;
}

/* Play `sec` seconds; lhz/rhz = 0 silences that channel. Returns 0 on
 * success (every byte accepted by the kernel). */
static int play(unsigned rate, double lhz, double rhz, double sec, const char *what)
{
	int fd = open_out(rate, 2);
	if (fd < 0)
		return 1;
	const unsigned frames_total = (unsigned)(rate * sec);
	int16_t buf[2 * 1024];
	double pl = 0, pr = 0;
	unsigned done = 0;
	size_t bytes = 0;
	double t0 = now_s(), next_report = 0.5;
	int rc = 0;
	printf("sndtest: %s: %.1f s at %u Hz (L %g Hz, R %g Hz)\n", what, sec, rate, lhz, rhz);
	while (done < frames_total) {
		unsigned n = frames_total - done;
		if (n > 1024)
			n = 1024;
		for (unsigned i = 0; i < n; i++) {
			/* 0.3 FS (-10.5 dBFS); short fade in/out to avoid clicks. */
			double env = 1.0;
			unsigned k = done + i, fade = rate / 100;
			if (k < fade)
				env = (double)k / fade;
			else if (frames_total - k < fade)
				env = (double)(frames_total - k) / fade;
			buf[2 * i] = lhz > 0 ? (int16_t)(9830.0 * env * sin(pl)) : 0;
			buf[2 * i + 1] = rhz > 0 ? (int16_t)(9830.0 * env * sin(pr)) : 0;
			pl += 2 * M_PI * lhz / rate;
			pr += 2 * M_PI * rhz / rate;
			if (pl > 2 * M_PI) pl -= 2 * M_PI;
			if (pr > 2 * M_PI) pr -= 2 * M_PI;
		}
		size_t want = (size_t)n * 4, off = 0;
		while (off < want) {
			ssize_t w = write(fd, (char *)buf + off, want - off);
			if (w <= 0) {
				printf("sndtest: write: %s (after %zu bytes)\n", w < 0 ? strerror(errno) : "0", bytes + off);
				rc = 1;
				goto out;
			}
			off += (size_t)w;
		}
		bytes += want;
		done += n;
		double t = now_s() - t0;
		if (t >= next_report) {
			printf("sndtest:   t=%.1fs queued %u B, written %zu B\n", t, delay_bytes(fd), bytes);
			next_report += 0.5;
		}
	}
	/* Drain: wait until the device has played what we queued. */
	{
		double t1 = now_s();
		unsigned d;
		/* Until the queue is (nearly) empty or stops shrinking: both
		 * backends keep a silence floor queued (virtio-sound its 16 KiB
		 * top-up, HDA its resync cushion), so "empty" may never come. */
		unsigned last = delay_bytes(fd);
		double moved = now_s();
		while ((d = delay_bytes(fd)) > 2048 && now_s() - t1 < 3.0 && now_s() - moved < 0.2) {
			if (d < last) {
				last = d;
				moved = now_s();
			}
			usleep(20000);
		}
		printf("sndtest:   %s done: %zu B in %.2f s (expected %.2f s), left queued %u B\n", what, bytes,
		       now_s() - t0, sec, d);
	}
out:
	close(fd);
	return rc;
}

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IOLBF, 0);
	const char *cmd = argc > 1 ? argv[1] : "all";
	if (!strcmp(cmd, "info")) {
		fputs(get_info(), stdout);
		return 0;
	}
	if (!strcmp(cmd, "tone")) {
		double hz = argc > 2 ? atof(argv[2]) : 440;
		double sec = argc > 3 ? atof(argv[3]) : 2;
		unsigned rate = argc > 4 ? (unsigned)atoi(argv[4]) : 48000;
		return play(rate, hz, hz, sec, "tone");
	}
	if (!strcmp(cmd, "lr")) {
		double sec = argc > 2 ? atof(argv[2]) : 1;
		int a = play(48000, 440, 0, sec, "left channel only");
		int b = play(48000, 0, 660, sec, "right channel only");
		return a || b;
	}
	if (strcmp(cmd, "all")) {
		fprintf(stderr, "usage: sndtest [info|tone [hz] [sec] [rate]|lr [sec]|all]\n");
		return 2;
	}

	char before[4096];
	snprintf(before, sizeof before, "%s", get_info());
	fputs(before, stdout);
	long long played0 = info_num(before, "played");
	long long under0 = info_num(before, "starved");

	int r1 = play(48000, 440, 440, 2.0, "440 Hz tone, both channels");
	int r2 = play(48000, 440, 0, 1.0, "left channel only (440 Hz)");
	int r3 = play(48000, 0, 660, 1.0, "right channel only (660 Hz)");
	int r4 = play(44100, 440, 440, 1.0, "440 Hz tone at 44.1 kHz");

	const char *after = get_info();
	fputs(after, stdout);
	long long played1 = info_num(after, "played");
	long long under1 = info_num(after, "starved");

	int ok = !r1 && !r2 && !r3 && !r4;
	if (played0 >= 0 && played1 >= 0) {
		/* The stream restarts (counters reset) on every SET_PARAMS, so only
		 * the last segment's progress is visible: 1 s at 44.1 kHz stereo. */
		long long need = 44100LL * 4 * 8 / 10;
		printf("sndtest: DMA progress: played %lld B since the last SET_PARAMS (need >= %lld), starved %lld->%lld (each playback end counts once)\n",
		       played1, need, under0, under1);
		if (played1 < need)
			ok = 0;
		(void)played0;
	}
	printf("sndtest: SUMMARY %s (tone %s, left %s, right %s, 44.1k %s)\n", ok ? "PASS" : "FAIL",
	       r1 ? "fail" : "ok", r2 ? "fail" : "ok", r3 ? "fail" : "ok", r4 ? "fail" : "ok");
	return ok ? 0 : 1;
}
