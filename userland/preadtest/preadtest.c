/*
 * preadtest — pread/pwrite on one shared block-device fd from many threads.
 *
 *   preadtest [-d DIR] [-n ITER]
 *
 * A loop device over a pattern file in DIR (default /tmp) is opened once;
 * 8 threads each pread() tagged 4 KiB blocks at their own offsets (and every
 * block's tag must be its own), then pwrite() fresh tags and read them back.
 * The descriptor's offset must be untouched afterwards. e2fsprogs loads
 * bitmaps this way; a seek/read/seek-back emulation returned other threads'
 * blocks. Exit status = failures. Builds on a host Linux too (needs root).
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define NTHR 8
#define BLK 4096
#define PER 16              /* blocks owned by each thread */
#define LOOP_SET_FD 0x4C00
#define LOOP_CLR_FD 0x4C01
#define LOOP_CTL_GET_FREE 0x4C82

static int dev;
static int iters = 2000;
static volatile int bad;

static void fill(uint8_t *b, uint64_t tag)
{
    for (int i = 0; i < BLK; i += 8) memcpy(b + i, &tag, 8);
}

static int check(const uint8_t *b, uint64_t tag)
{
    uint64_t t;
    for (int i = 0; i < BLK; i += 8) {
        memcpy(&t, b + i, 8);
        if (t != tag) return 0;
    }
    return 1;
}

static void *worker(void *arg)
{
    long id = (long)arg;
    uint8_t *b = aligned_alloc(BLK, BLK);
    for (int it = 0; it < iters && !bad; it++) {
        int k = it % PER;
        off_t off = (off_t)(id * PER + k) * BLK;
        uint64_t tag = ((uint64_t)id << 48) | (uint64_t)k;
        if (pread(dev, b, BLK, off) != BLK || !check(b, tag)) {
            fprintf(stderr, "thread %ld: read of block %d returned wrong data\n", id, k);
            bad = 1; break;
        }
        if (it % 4 == 3) {   /* rewrite with a new generation, read it back */
            uint64_t t2 = tag | ((uint64_t)(it & 0xffff) << 24) | (1ull << 47);
            fill(b, t2);
            if (pwrite(dev, b, BLK, off) != BLK) { bad = 1; break; }
            memset(b, 0, BLK);
            if (pread(dev, b, BLK, off) != BLK || !check(b, t2)) {
                fprintf(stderr, "thread %ld: readback of block %d wrong\n", id, k);
                bad = 1; break;
            }
            fill(b, tag);   /* restore for the next lap */
            if (pwrite(dev, b, BLK, off) != BLK) { bad = 1; break; }
        }
    }
    free(b);
    return NULL;
}

int main(int argc, char **argv)
{
    const char *dir = "/tmp";
    int fails = 0;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-d") && i + 1 < argc) dir = argv[++i];
        else if (!strcmp(argv[i], "-n") && i + 1 < argc) iters = atoi(argv[++i]);
        else { fprintf(stderr, "usage: preadtest [-d dir] [-n iter]\n"); return 2; }
    }
    char path[256];
    snprintf(path, sizeof path, "%s/preadtest.%d", dir, (int)getpid());
    int bf = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (bf < 0) { perror("backing"); return 1; }
    uint8_t *b = aligned_alloc(BLK, BLK);
    for (long id = 0; id < NTHR; id++)
        for (int k = 0; k < PER; k++) {
            fill(b, ((uint64_t)id << 48) | (uint64_t)k);
            if (write(bf, b, BLK) != BLK) { perror("write backing"); return 1; }
        }
    fsync(bf);

    int ctl = open("/dev/loop-control", O_RDWR);
    int n = ctl >= 0 ? ioctl(ctl, LOOP_CTL_GET_FREE) : 0;
    if (n < 0) n = 0;
    char lp[64];
    snprintf(lp, sizeof lp, "/dev/loop%d", n);
    dev = open(lp, O_RDWR);
    if (dev < 0 || ioctl(dev, LOOP_SET_FD, bf) < 0) {
        printf("preadtest: SKIP (no usable %s: %s)\n", lp, strerror(errno));
        unlink(path);
        return 0;
    }
    close(bf);

    off_t pos = lseek(dev, 12345 * 512, SEEK_SET);
    pthread_t t[NTHR];
    for (long i = 0; i < NTHR; i++) pthread_create(&t[i], NULL, worker, (void *)i);
    for (int i = 0; i < NTHR; i++) pthread_join(t[i], NULL);
    if (bad) { printf("FAIL threaded_pread_pwrite\n"); fails++; }
    else printf("PASS threaded_pread_pwrite (%d threads x %d)\n", NTHR, iters);

    off_t after = lseek(dev, 0, SEEK_CUR);
    if (after != pos) { printf("FAIL fd_offset_unchanged (%ld -> %ld)\n", (long)pos, (long)after); fails++; }
    else printf("PASS fd_offset_unchanged\n");

    ioctl(dev, LOOP_CLR_FD, 0);
    close(dev);
    unlink(path);
    printf("preadtest: %d failed\n", fails);
    return fails;
}
