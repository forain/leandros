// serrx: receive one file over the serial console (stdin/stdout), framed,
// CRC-checked and acknowledged, for hosts with no network (Raspberry Pi 5).
// The sender is scripts/pi-serial-push.py.
//
//   serrx -V             print "@SERRX1." (version probe)
//   serrx -s <file>      print "@SHA<sha256 hex>." of a file
//   serrx <path>         receive into <path>.part, verify, fsync, rename
//
// Frames (host -> serrx), one per line, all printable ASCII so nothing in the
// console path (Ctrl-T run-queue dump, ISIG bytes, NUL drop) can touch them:
//   #<T><seq:8 hex><len:4 hex><crc32:8 hex>:<base64 of len bytes>\n
//   T = H header (payload "<size hex> <sha256 hex>"), D data, E end, X abort.
// Replies (serrx -> host), CR-prefixed and newline-free so the framebuffer
// console overwrites one line instead of scrolling:
//   \r@A<next seq>.   cumulative ACK (go-back-N)
//   \r@N<next seq>.   NAK: resend from <next seq>
//   \r@OK<sha>.  \r@BAD<sha>.  \r@ERR<text>.  \r@TIMEOUT.  \r@ABORT.
// The tty goes raw while serrx runs and is restored on every exit path; with
// no frame for IDLE_SECS it gives up, so a vanished sender cannot wedge the
// console.
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <termios.h>
#include <unistd.h>

#define IDLE_SECS 30
#define MAXLINE 8192
#define MAXDATA 4096

static struct termios saved;
static int have_saved;
static char part[4096];
static int partfd = -1;

static void say(const char *s) { (void)!write(1, s, strlen(s)); }

static void restore(void) {
    if (have_saved) tcsetattr(0, TCSAFLUSH, &saved);
}

static void bail(const char *msg, int code) {
    if (partfd >= 0) { close(partfd); unlink(part); }
    say("\r");
    say(msg);
    say("\r\n");
    restore();
    _exit(code);
}

static void on_alarm(int s) { (void)s; bail("@TIMEOUT.", 3); }
static void on_term(int s) { (void)s; bail("@ABORT.", 4); }

// ---- crc32 (IEEE) ----
static uint32_t crctab[256];
static void crc_init(void) {
    for (uint32_t i = 0; i < 256; i++) {
        uint32_t c = i;
        for (int k = 0; k < 8; k++) c = c & 1 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
        crctab[i] = c;
    }
}
static uint32_t crc32(const uint8_t *p, size_t n) {
    uint32_t c = 0xFFFFFFFFu;
    while (n--) c = crctab[(c ^ *p++) & 0xFF] ^ (c >> 8);
    return c ^ 0xFFFFFFFFu;
}

// ---- sha256 ----
typedef struct { uint32_t h[8]; uint64_t len; uint8_t buf[64]; size_t n; } sha256;
static const uint32_t K[64] = {
    0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,0x923f82a4,0xab1c5ed5,
    0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,
    0xe49b69c1,0xefbe4786,0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
    0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,0x06ca6351,0x14292967,
    0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,
    0xa2bfe8a1,0xa81a664b,0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
    0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,0x5b9cca4f,0x682e6ff3,
    0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2};
#define ROR(x, n) (((x) >> (n)) | ((x) << (32 - (n))))
static void sha_block(sha256 *s, const uint8_t *b) {
    uint32_t w[64], a, bb, c, d, e, f, g, h;
    for (int i = 0; i < 16; i++)
        w[i] = (uint32_t)b[4*i] << 24 | (uint32_t)b[4*i+1] << 16 | (uint32_t)b[4*i+2] << 8 | b[4*i+3];
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = ROR(w[i-15], 7) ^ ROR(w[i-15], 18) ^ (w[i-15] >> 3);
        uint32_t s1 = ROR(w[i-2], 17) ^ ROR(w[i-2], 19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    a = s->h[0]; bb = s->h[1]; c = s->h[2]; d = s->h[3]; e = s->h[4]; f = s->h[5]; g = s->h[6]; h = s->h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t t1 = h + (ROR(e, 6) ^ ROR(e, 11) ^ ROR(e, 25)) + ((e & f) ^ (~e & g)) + K[i] + w[i];
        uint32_t t2 = (ROR(a, 2) ^ ROR(a, 13) ^ ROR(a, 22)) + ((a & bb) ^ (a & c) ^ (bb & c));
        h = g; g = f; f = e; e = d + t1; d = c; c = bb; bb = a; a = t1 + t2;
    }
    s->h[0] += a; s->h[1] += bb; s->h[2] += c; s->h[3] += d;
    s->h[4] += e; s->h[5] += f; s->h[6] += g; s->h[7] += h;
}
static void sha_init(sha256 *s) {
    static const uint32_t iv[8] = {0x6a09e667,0xbb67ae85,0x3c6ef372,0xa54ff53a,
                                   0x510e527f,0x9b05688c,0x1f83d9ab,0x5be0cd19};
    memcpy(s->h, iv, sizeof iv); s->len = 0; s->n = 0;
}
static void sha_update(sha256 *s, const uint8_t *p, size_t n) {
    s->len += n;
    while (n) {
        size_t k = 64 - s->n < n ? 64 - s->n : n;
        memcpy(s->buf + s->n, p, k); s->n += k; p += k; n -= k;
        if (s->n == 64) { sha_block(s, s->buf); s->n = 0; }
    }
}
static void sha_hex(sha256 *s, char out[65]) {
    uint64_t bits = s->len * 8;
    uint8_t pad = 0x80, z = 0;
    sha_update(s, &pad, 1);
    while (s->n != 56) sha_update(s, &z, 1);
    uint8_t L[8];
    for (int i = 0; i < 8; i++) L[i] = (uint8_t)(bits >> (56 - 8 * i));
    sha_update(s, L, 8);
    for (int i = 0; i < 8; i++) sprintf(out + 8 * i, "%08x", s->h[i]);
    out[64] = 0;
}
static int sha_file(const char *path, char out[65]) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    static uint8_t b[65536];
    sha256 s; sha_init(&s);
    ssize_t r;
    while ((r = read(fd, b, sizeof b)) > 0) sha_update(&s, b, (size_t)r);
    close(fd);
    if (r < 0) return -1;
    sha_hex(&s, out);
    return 0;
}

// ---- base64 ----
static int b64v(int c) {
    if (c >= 'A' && c <= 'Z') return c - 'A';
    if (c >= 'a' && c <= 'z') return c - 'a' + 26;
    if (c >= '0' && c <= '9') return c - '0' + 52;
    if (c == '+') return 62;
    if (c == '/') return 63;
    return -1;
}
// Returns decoded length or -1.
static long b64dec(const char *in, size_t n, uint8_t *out, size_t cap) {
    if (n % 4) return -1;
    size_t o = 0;
    for (size_t i = 0; i < n; i += 4) {
        int v[4], pads = 0;
        for (int k = 0; k < 4; k++) {
            if (in[i+k] == '=' && i + 4 == n && k >= 2) { v[k] = 0; pads++; continue; }
            if (pads || (v[k] = b64v((unsigned char)in[i+k])) < 0) return -1;
        }
        uint32_t x = (uint32_t)v[0] << 18 | v[1] << 12 | v[2] << 6 | v[3];
        uint8_t t[3] = {(uint8_t)(x >> 16), (uint8_t)(x >> 8), (uint8_t)x};
        if (o + 3 - pads > cap) return -1;
        memcpy(out + o, t, 3 - pads); o += 3 - pads;
    }
    return (long)o;
}

static int hexn(const char *p, int n, uint32_t *out) {
    uint32_t v = 0;
    for (int i = 0; i < n; i++) {
        int c = p[i], d;
        if (c >= '0' && c <= '9') d = c - '0';
        else if (c >= 'a' && c <= 'f') d = c - 'a' + 10;
        else return -1;
        v = v << 4 | (uint32_t)d;
    }
    *out = v;
    return 0;
}

static uint32_t expect;   // next sequence number wanted
static uint32_t nakked = 0xFFFFFFFFu;

static void reply(char kind, uint32_t seq) {
    char b[24];
    snprintf(b, sizeof b, "\r@%c%08x.", kind, seq);
    say(b);
}

static int write_all(int fd, const uint8_t *p, size_t n) {
    while (n) {
        ssize_t w = write(fd, p, n);
        if (w < 0) { if (errno == EINTR) continue; return -1; }
        p += w; n -= (size_t)w;
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "-V")) { say("@SERRX1.\n"); return 0; }
    if (argc == 3 && !strcmp(argv[1], "-s")) {
        char h[65];
        if (sha_file(argv[2], h)) { say("@ERRread.\n"); return 1; }
        printf("@SHA%s.\n", h);
        return 0;
    }
    if (argc != 2 || argv[1][0] == '-') {
        fprintf(stderr, "usage: serrx <path> | serrx -V | serrx -s <file>\n");
        return 2;
    }
    const char *path = argv[1];
    if (strlen(path) + 6 > sizeof part) { fprintf(stderr, "serrx: path too long\n"); return 2; }
    snprintf(part, sizeof part, "%s.part", path);
    crc_init();

    if (tcgetattr(0, &saved) == 0) {
        have_saved = 1;
        struct termios t = saved;
        t.c_iflag &= ~(IGNBRK | BRKINT | PARMRK | ISTRIP | INLCR | IGNCR | ICRNL | IXON | IXOFF);
        t.c_oflag &= ~OPOST;
        t.c_lflag &= ~(ECHO | ECHONL | ICANON | ISIG | IEXTEN);
        t.c_cflag &= ~(CSIZE | PARENB);
        t.c_cflag |= CS8;
        t.c_cc[VMIN] = 1; t.c_cc[VTIME] = 0;
        tcsetattr(0, TCSAFLUSH, &t);
    }
    signal(SIGALRM, on_alarm);
    signal(SIGTERM, on_term);
    signal(SIGHUP, on_term);
    signal(SIGINT, on_term);

    static char line[MAXLINE + 1];
    static uint8_t data[MAXDATA];
    static uint8_t in[4096];
    size_t ll = 0;
    int inframe = 0, have_hdr = 0, at_exp = 0;
    uint64_t size = 0, got = 0;
    char want_sha[65] = {0};

    // Touch the buffers first: the console read fast path returns EFAULT for
    // a destination page that is mapped but not yet faulted in (.bss).
    memset(line, 0, sizeof line); memset(data, 0, sizeof data); memset(in, 0, sizeof in);
    say("\r@READY1.");
    alarm(IDLE_SECS);
    for (;;) {
        ssize_t r = read(0, in, sizeof in);
        if (r < 0) {
            if (errno == EINTR) continue;
            if (errno == EAGAIN) {   // the shell may leave the console O_NONBLOCK
                struct pollfd p = {0, POLLIN, 0};
                poll(&p, 1, 1000);
                continue;
            }
            char m[32]; snprintf(m, sizeof m, "@ERRread%d.", errno);
            bail(m, 1);
        }
        if (r == 0) bail("@ERReof.", 1);
        alarm(IDLE_SECS);
        for (ssize_t i = 0; i < r; i++) {
            char c = (char)in[i];
            if (c == '#') { inframe = 1; ll = 0; continue; }   // resync on every '#'
            if (!inframe) continue;
            if (c != '\n' && c != '\r') {
                if (ll < MAXLINE) line[ll++] = c; else inframe = 0;
                if (!inframe) goto bad;
                continue;
            }
            inframe = 0;
            line[ll] = 0;
            // T seq8 len4 crc8 ':' payload
            uint32_t seq, len, crc;
            at_exp = 0;
            if (ll < 22 || line[21] != ':' || hexn(line + 1, 8, &seq) || hexn(line + 9, 4, &len)
                || hexn(line + 13, 8, &crc) || len > MAXDATA)
                goto bad;
            char T = line[0];
            if (T == 'X') bail("@ABORT.", 4);
            if (seq != expect) {
                // Duplicate of something already taken (its ACK was lost):
                // repeat the cumulative ACK. Ahead of us: dropped, the sender
                // rewinds on the NAK or its timeout.
                if (seq < expect) reply('A', expect);
                continue;
            }
            at_exp = 1;
            long n = b64dec(line + 22, ll - 22, data, sizeof data);
            if (n < 0 || (uint32_t)n != len || crc32(data, (size_t)n) != crc) goto bad;
            if (T == 'H') {
                char tmp[128];
                if (have_hdr || n >= (long)sizeof tmp) goto bad;
                memcpy(tmp, data, (size_t)n); tmp[n] = 0;
                unsigned long long sz;
                if (sscanf(tmp, "%llx %64s", &sz, want_sha) != 2 || strlen(want_sha) != 64) goto bad;
                size = sz;
                partfd = open(part, O_WRONLY | O_CREAT | O_TRUNC, 0644);
                if (partfd < 0) bail("@ERRopen.", 1);
                have_hdr = 1;
            } else if (T == 'D') {
                if (!have_hdr || got + (uint64_t)n > size) goto bad;
                if (write_all(partfd, data, (size_t)n)) bail("@ERRwrite.", 1);
                got += (uint64_t)n;
            } else if (T == 'E') {
                if (!have_hdr || got != size) bail("@ERRshort.", 1);
                alarm(0);
                if (fsync(partfd) || close(partfd)) { partfd = -1; unlink(part); bail("@ERRfsync.", 1); }
                partfd = -1;
                char h[65];
                if (sha_file(part, h)) { unlink(part); bail("@ERRreread.", 1); }
                if (strcmp(h, want_sha)) {
                    unlink(part);
                    char m[96]; snprintf(m, sizeof m, "@BAD%s.", h);
                    bail(m, 5);
                }
                if (rename(part, path)) { unlink(part); bail("@ERRrename.", 1); }
                // fsync the directory so the rename survives a power cut.
                char dir[4096]; snprintf(dir, sizeof dir, "%s", path);
                char *sl = strrchr(dir, '/');
                if (sl == dir) sl[1] = 0; else if (sl) *sl = 0; else strcpy(dir, ".");
                int dfd = open(dir, O_RDONLY);
                if (dfd >= 0) { fsync(dfd); close(dfd); }
                reply('A', expect + 1);
                char m[96]; snprintf(m, sizeof m, "@OK%s.", h);
                bail(m, 0);
            } else goto bad;
            expect++;
            nakked = 0xFFFFFFFFu;
            reply('A', expect);
            continue;
        bad:
            inframe = 0;
            if (nakked != expect || at_exp) { reply('N', expect); nakked = expect; }
        }
    }
}
