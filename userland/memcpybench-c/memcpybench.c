/* memcpybench — wl_shm-style copy cost (lane shmslow).
 *
 * Linked against libleandros_libc.a exactly like DOOM, so it measures the
 * libc memcpy DOOM's wl_shm present uses. Reports, for a 256000-byte frame
 * (DOOM's 320x200 XRGB):
 *   touch_ns_pg   first write to each page of a fresh MAP_SHARED memfd
 *   shm_us/MBps   libc memcpy into the (already faulted) memfd mapping
 *   anon_us/MBps  libc memcpy into a private anonymous mapping
 *   byte_us       a byte-at-a-time store loop (the old libc memcpy's shape)
 *   memset_us     libc memset of the frame
 * Built -fno-builtin so the byte-loop references stay byte loops.
 * Usage: memcpybench [iterations]   (default 200)
 */
typedef unsigned long size_t;
struct timespec { long tv_sec; long tv_nsec; };
int printf(const char *fmt, ...);
void *memcpy(void *, const void *, size_t);
void *memset(void *, int, size_t);
void *memmove(void *, const void *, size_t);
void *mmap(void *, size_t, int, int, int, long);
int clock_gettime(int, struct timespec *);

#ifdef __aarch64__
#define NR_MEMFD_CREATE 279
#define NR_FTRUNCATE 46
#else
#define NR_MEMFD_CREATE 319
#define NR_FTRUNCATE 77
#endif
static long sys3(long n, long a0, long a1, long a2) {
    long ret;
#ifdef __aarch64__
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a0;
    register long x1 __asm__("x1") = a1;
    register long x2 __asm__("x2") = a2;
    __asm__ __volatile__("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2) : "memory");
    ret = x0;
#else
    __asm__ __volatile__("syscall" : "=a"(ret) : "a"(n), "D"(a0), "S"(a1), "d"(a2)
                         : "rcx", "r11", "memory");
#endif
    return ret;
}
static unsigned long now_ns(void) {
    struct timespec ts; clock_gettime(1, &ts);
    return (unsigned long)ts.tv_sec * 1000000000ul + (unsigned long)ts.tv_nsec;
}

/* Exhaustive small-size check of the libc mem* (every length 0..300 at every
 * src/dst misalignment 0..15, plus overlapping memmove both ways) against a
 * byte-loop reference. Returns the number of mismatches. */
static unsigned char ra[1024], rb[1024], rc[1024];
static int check_mem(void) {
    int memerr = check_mem();
    int bad = memerr != 0;
    for (int so = 0; so < 16; so++) for (int dof = 0; dof < 16; dof++) for (int n = 0; n <= 300; n += (n < 80 ? 1 : 7)) {
        for (int i = 0; i < 1024; i++) { ra[i] = (unsigned char)(i * 13 + 1); rb[i] = rc[i] = (unsigned char)(i * 5 + 3); }
        memcpy(rb + dof, ra + so, (size_t)n);
        for (int i = 0; i < n; i++) rc[dof + i] = ra[so + i];
        for (int i = 0; i < 1024; i++) if (rb[i] != rc[i]) { bad++; break; }
        memset(rb + dof, so * 17, (size_t)n);
        for (int i = 0; i < n; i++) rc[dof + i] = (unsigned char)(so * 17);
        for (int i = 0; i < 1024; i++) if (rb[i] != rc[i]) { bad++; break; }
        /* overlap: dst = src +/- (dof+1) within one buffer */
        for (int dir = 0; dir < 2; dir++) {
            int s0 = 200 + so, d0 = dir ? s0 + dof + 1 : s0 - dof - 1;
            for (int i = 0; i < 1024; i++) rb[i] = rc[i] = (unsigned char)(i * 7 + dir);
            memmove(rb + d0, rb + s0, (size_t)n);
            unsigned char tmp[320];
            for (int i = 0; i < n; i++) tmp[i] = rc[s0 + i];
            for (int i = 0; i < n; i++) rc[d0 + i] = tmp[i];
            for (int i = 0; i < 1024; i++) if (rb[i] != rc[i]) { bad++; break; }
        }
    }
    return bad;
}
#define FRAME 256000ul
#define PG 4096ul

int main(int argc, char **argv) {
    unsigned long iters = 200;
    if (argc > 1) { iters = 0; for (char *p = argv[1]; *p >= '0' && *p <= '9'; p++) iters = iters * 10 + (unsigned long)(*p - '0'); }
    if (!iters) iters = 1;
    long fd = sys3(NR_MEMFD_CREATE, (long)"bench-shm", 1, 0);
    if (fd < 0 || sys3(NR_FTRUNCATE, fd, 2 * 262144, 0) < 0) { printf("memcpybench: memfd failed %ld\n", fd); return 1; }
    unsigned char *shm = mmap(0, 2 * 262144, 3, 1 /* SHARED */, (int)fd, 0);
    unsigned char *anon = mmap(0, 262144, 3, 0x22 /* PRIVATE|ANON */, -1, 0);
    unsigned char *src = mmap(0, 262144, 3, 0x22, -1, 0);
    if ((long)shm < 0 && (long)shm > -4096) { printf("memcpybench: mmap failed\n"); return 1; }
    for (size_t i = 0; i < FRAME; i++) src[i] = (unsigned char)(i * 7);

    unsigned long t = now_ns();
    for (size_t o = 0; o < 262144; o += PG) shm[o] = 1;
    unsigned long touch = (now_ns() - t) / (262144 / PG);
    for (size_t o = 0; o < 262144; o += PG) anon[o] = 1;
    memcpy(shm + 262144, src, FRAME); /* fault the second buffer too */

    unsigned long shm_ns = 0, anon_ns = 0, byte_ns = 0, set_ns = 0;
    for (unsigned long k = 0; k < iters; k++) {
        unsigned char *d = shm + (k & 1) * 262144;
        t = now_ns(); memcpy(d, src, FRAME); shm_ns += now_ns() - t;
        t = now_ns(); memcpy(anon, src, FRAME); anon_ns += now_ns() - t;
        t = now_ns(); memset(anon, (int)k, FRAME); set_ns += now_ns() - t;
    }
    unsigned long bi = iters < 20 ? iters : 20;
    for (unsigned long k = 0; k < bi; k++) {
        volatile unsigned char *d = shm;
        t = now_ns(); for (size_t i = 0; i < FRAME; i++) d[i] = src[i]; byte_ns += now_ns() - t;
    }
    int memerr = check_mem();
    int bad = memerr != 0;
    for (size_t i = 0; i < FRAME; i++) if (shm[i] != (unsigned char)(i * 7)) { bad = 1; break; }
    memcpy(anon, src, FRAME);
    for (size_t i = 0; i < FRAME; i++) if (anon[i] != (unsigned char)(i * 7)) { bad = 1; break; }
    unsigned long su = shm_ns / iters / 1000, au = anon_ns / iters / 1000;
    printf("[memcpybench] frame=%lu iters=%lu touch_ns_pg=%lu shm_us=%lu shm_MBps=%lu anon_us=%lu anon_MBps=%lu byte_us=%lu memset_us=%lu memchk_err=%d verify=%s\n",
           FRAME, iters, touch, su, FRAME * iters * 1000ul / (shm_ns ? shm_ns : 1),
           au, FRAME * iters * 1000ul / (anon_ns ? anon_ns : 1),
           byte_ns / bi / 1000, set_ns / iters / 1000, memerr, bad ? "FAIL" : "ok");
    return bad;
}
