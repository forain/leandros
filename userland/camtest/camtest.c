// camtest: grab frames from /dev/video0 through the V4L2 mmap streaming API.
//
//   camtest [N] [mjpg|yuyv WxH] [-o FILE]   capture N frames (default 30),
//        print size + FNV-1a per frame (the hash uvc.selftest logs), write the
//        last frame to FILE (default /tmp/cam.jpg for MJPEG, /tmp/cam.raw else)
//   camtest --probe   regression check: with no camera the node must refuse
//        open with ENODEV (or be absent); with one, QUERYCAP/ENUM/G_FMT must answer.
//
// Built against the real linux/videodev2.h; the asserts pin the ioctl numbers
// (and so the struct sizes) the kernel side (kernel/src/v4l2.rs) answers.
#include <errno.h>
#include <fcntl.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>

_Static_assert(VIDIOC_QUERYCAP == 0x80685600, "querycap");
_Static_assert(VIDIOC_ENUM_FMT == 0xC0405602, "enum_fmt");
_Static_assert(VIDIOC_S_FMT == 0xC0D05605, "s_fmt");
_Static_assert(VIDIOC_REQBUFS == 0xC0145608, "reqbufs");
_Static_assert(VIDIOC_QUERYBUF == 0xC0585609, "querybuf");
_Static_assert(VIDIOC_DQBUF == 0xC0585611, "dqbuf");
_Static_assert(VIDIOC_G_PARM == 0xC0CC5615, "g_parm");
_Static_assert(VIDIOC_ENUMINPUT == 0xC050561A, "enuminput");
_Static_assert(VIDIOC_ENUM_FRAMESIZES == 0xC02C564A, "framesizes");
_Static_assert(VIDIOC_ENUM_FRAMEINTERVALS == 0xC034564B, "frameintervals");

#define NBUF 4

static uint32_t fnv1a(const uint8_t *d, size_t n) {
    uint32_t h = 0x811C9DC5u;
    for (size_t i = 0; i < n; i++) h = (h ^ d[i]) * 0x01000193u;
    return h;
}

static int xioctl(int fd, unsigned long req, void *arg) {
    int r;
    do r = ioctl(fd, req, arg); while (r < 0 && errno == EINTR);
    return r;
}

static void fourcc(uint32_t f, char out[5]) {
    for (int i = 0; i < 4; i++) out[i] = (char)((f >> (8 * i)) & 0xFF);
    out[4] = 0;
}

static int describe(int fd) {
    struct v4l2_capability cap;
    memset(&cap, 0, sizeof cap);
    if (xioctl(fd, VIDIOC_QUERYCAP, &cap) < 0) { perror("camtest: QUERYCAP"); return -1; }
    printf("camtest: %s / %s / %s caps %08x\n", cap.driver, cap.card, cap.bus_info, cap.device_caps);
    if (!(cap.device_caps & V4L2_CAP_VIDEO_CAPTURE) || !(cap.device_caps & V4L2_CAP_STREAMING)) {
        fprintf(stderr, "camtest: not a streaming capture device\n");
        return -1;
    }
    for (int i = 0;; i++) {
        struct v4l2_fmtdesc fd_ = { .index = (uint32_t)i, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE };
        if (xioctl(fd, VIDIOC_ENUM_FMT, &fd_) < 0) break;
        char cc[5]; fourcc(fd_.pixelformat, cc);
        printf("camtest: format %d %s (%s):", i, cc, fd_.description);
        for (int j = 0;; j++) {
            struct v4l2_frmsizeenum fs = { .index = (uint32_t)j, .pixel_format = fd_.pixelformat };
            if (xioctl(fd, VIDIOC_ENUM_FRAMESIZES, &fs) < 0) break;
            struct v4l2_frmivalenum fi = { .index = 0, .pixel_format = fd_.pixelformat,
                                           .width = fs.discrete.width, .height = fs.discrete.height };
            unsigned fps = 0;
            if (xioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS, &fi) == 0 && fi.discrete.numerator)
                fps = fi.discrete.denominator / fi.discrete.numerator;
            printf(" %ux%u@%u", fs.discrete.width, fs.discrete.height, fps);
        }
        printf("\n");
    }
    return 0;
}

static int probe(void) {
    int fd = open("/dev/video0", O_RDWR);
    if (fd < 0) {
        if (errno == ENODEV || errno == ENOENT || errno == ENXIO) {
            printf("camtest: probe: no camera (%s) PASS\n", strerror(errno));
            return 0;
        }
        printf("camtest: probe: open failed unexpectedly: %s FAIL\n", strerror(errno));
        return 1;
    }
    int bad = describe(fd) < 0;
    struct v4l2_format f = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE };
    if (xioctl(fd, VIDIOC_G_FMT, &f) < 0 || f.fmt.pix.width == 0) bad = 1;
    struct v4l2_buffer b = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP };
    if (xioctl(fd, VIDIOC_DQBUF, &b) == 0) bad = 1; // not streaming: must fail
    close(fd);
    printf("camtest: probe: camera present, ioctls %s\n", bad ? "FAIL" : "PASS");
    return bad;
}

int main(int argc, char **argv) {
    int frames = 30;
    uint32_t want_fmt = V4L2_PIX_FMT_MJPEG, w = 640, h = 480;
    const char *out = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--probe")) return probe();
        if (!strcmp(argv[i], "-o") && i + 1 < argc) { out = argv[++i]; continue; }
        if (!strcasecmp(argv[i], "mjpg") || !strcasecmp(argv[i], "mjpeg")) { want_fmt = V4L2_PIX_FMT_MJPEG; continue; }
        if (!strcasecmp(argv[i], "yuyv")) { want_fmt = V4L2_PIX_FMT_YUYV; continue; }
        if (strchr(argv[i], 'x') && sscanf(argv[i], "%ux%u", &w, &h) == 2) continue;
        frames = atoi(argv[i]);
    }
    if (!out) out = want_fmt == V4L2_PIX_FMT_MJPEG ? "/tmp/cam.jpg" : "/tmp/cam.raw";

    int fd = open("/dev/video0", O_RDWR | O_NONBLOCK);
    if (fd < 0) { perror("camtest: open /dev/video0"); return 1; }
    if (describe(fd) < 0) return 1;

    struct v4l2_format f = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE };
    f.fmt.pix.width = w; f.fmt.pix.height = h; f.fmt.pix.pixelformat = want_fmt;
    f.fmt.pix.field = V4L2_FIELD_NONE;
    if (xioctl(fd, VIDIOC_S_FMT, &f) < 0) { perror("camtest: S_FMT"); return 1; }
    char cc[5]; fourcc(f.fmt.pix.pixelformat, cc);
    struct v4l2_streamparm parm = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE };
    unsigned fps = 0;
    if (xioctl(fd, VIDIOC_G_PARM, &parm) == 0 && parm.parm.capture.timeperframe.numerator)
        fps = parm.parm.capture.timeperframe.denominator / parm.parm.capture.timeperframe.numerator;
    printf("camtest: using %s %ux%u sizeimage %u @%u fps\n", cc, f.fmt.pix.width, f.fmt.pix.height,
           f.fmt.pix.sizeimage, fps);

    struct v4l2_requestbuffers rb = { .count = NBUF, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP };
    if (xioctl(fd, VIDIOC_REQBUFS, &rb) < 0 || rb.count < 2) { perror("camtest: REQBUFS"); return 1; }
    void *map[NBUF] = {0};
    size_t len[NBUF] = {0};
    for (unsigned i = 0; i < rb.count && i < NBUF; i++) {
        struct v4l2_buffer b = { .index = i, .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP };
        if (xioctl(fd, VIDIOC_QUERYBUF, &b) < 0) { perror("camtest: QUERYBUF"); return 1; }
        len[i] = b.length;
        map[i] = mmap(NULL, b.length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, b.m.offset);
        if (map[i] == MAP_FAILED) { perror("camtest: mmap"); return 1; }
        if (xioctl(fd, VIDIOC_QBUF, &b) < 0) { perror("camtest: QBUF"); return 1; }
    }
    int type = V4L2_BUF_TYPE_VIDEO_CAPTURE;
    if (xioctl(fd, VIDIOC_STREAMON, &type) < 0) { perror("camtest: STREAMON"); return 1; }

    int got = 0, jpeg = 0, errs = 0;
    uint32_t prev = 0, distinct = 0;
    static uint8_t last[8 << 20];
    size_t last_n = 0;
    while (got < frames) {
        struct pollfd p = { .fd = fd, .events = POLLIN };
        int pr = poll(&p, 1, 3000);
        if (pr == 0) { fprintf(stderr, "camtest: timeout after %d frame(s)\n", got); break; }
        if (pr < 0) { if (errno == EINTR) continue; perror("camtest: poll"); break; }
        struct v4l2_buffer b = { .type = V4L2_BUF_TYPE_VIDEO_CAPTURE, .memory = V4L2_MEMORY_MMAP };
        if (xioctl(fd, VIDIOC_DQBUF, &b) < 0) {
            if (errno == EAGAIN) continue;
            perror("camtest: DQBUF");
            break;
        }
        const uint8_t *d = map[b.index];
        size_t n = b.bytesused < len[b.index] ? b.bytesused : len[b.index];
        uint32_t h32 = fnv1a(d, n);
        int soi = n >= 2 && d[0] == 0xFF && d[1] == 0xD8;
        jpeg += soi;
        if (b.flags & V4L2_BUF_FLAG_ERROR) errs++;
        if (got == 0 || h32 != prev) distinct++;
        prev = h32;
        printf("camtest: frame %d seq %u bytes %zu fnv %08x head %02x%02x ts %ld.%06ld%s\n", got, b.sequence, n, h32,
               n > 0 ? d[0] : 0, n > 1 ? d[1] : 0, (long)b.timestamp.tv_sec, (long)b.timestamp.tv_usec,
               (b.flags & V4L2_BUF_FLAG_ERROR) ? " ERR" : "");
        if (n <= sizeof last) { memcpy(last, d, n); last_n = n; }
        got++;
        if (xioctl(fd, VIDIOC_QBUF, &b) < 0) { perror("camtest: QBUF"); break; }
    }
    xioctl(fd, VIDIOC_STREAMOFF, &type);
    if (last_n) {
        FILE *o = fopen(out, "wb");
        if (o) { fwrite(last, 1, last_n, o); fclose(o); printf("camtest: wrote %zu bytes to %s\n", last_n, out); }
    }
    for (int i = 0; i < NBUF; i++) if (map[i] && map[i] != MAP_FAILED) munmap(map[i], len[i]);
    close(fd);
    int ok = got == frames && distinct > 1 && (want_fmt != V4L2_PIX_FMT_MJPEG || jpeg == got);
    printf("camtest: SUMMARY %s frames %d/%d jpeg %d distinct %u errors %d\n", ok ? "PASS" : "FAIL", got, frames,
           jpeg, distinct, errs);
    return ok ? 0 : 1;
}
