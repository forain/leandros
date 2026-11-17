// execdev: an ET_EXEC image whose segments sit at the QEMU virt device
// addresses (GICD 0x8000000, GICC 0x8010000, UART 0x9000000) must run.
//
// Every aarch64 TTBR0 root used to carry kernel-only identity pages at those
// three addresses, which silently shadowed a user mapping of the same page: a
// static binary larger than 128 MiB (MAME linked at lld's default image base
// 0x200000) took a permission fault at 0x8010000 for ever (lane execfault).
// The test writes a tiny hand-made ELF with one one-page RX PT_LOAD at each of
// the three addresses (the code hops 0x8000000 -> 0x8010000 -> 0x9000000 and
// exits 42), runs it from f2fs (demand-paged exec path) and checks the status.
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/wait.h>

#define NSEG 3
static const uint64_t VA[NSEG] = { 0x8000000, 0x8010000, 0x9000000 };

#if defined(__aarch64__)
#define EM 183
static const uint8_t C0[] = { 0x00, 0x40, 0x00, 0x14 };                  // b 0x8010000
static const uint8_t C1[] = { 0x00, 0xc0, 0x3f, 0x14 };                  // b 0x9000000
static const uint8_t C2[] = { 0x40, 0x05, 0x80, 0xd2,                    // mov x0,#42
                              0xa8, 0x0b, 0x80, 0xd2,                    // mov x8,#93
                              0x01, 0x00, 0x00, 0xd4 };                  // svc #0
#elif defined(__x86_64__)
#define EM 62
static const uint8_t C0[] = { 0xe9, 0xfb, 0xff, 0x00, 0x00 };            // jmp 0x8010000
static const uint8_t C1[] = { 0xe9, 0xfb, 0xff, 0xfe, 0x00 };            // jmp 0x9000000
static const uint8_t C2[] = { 0xbf, 0x2a, 0x00, 0x00, 0x00,              // mov edi,42
                              0xb8, 0x3c, 0x00, 0x00, 0x00,              // mov eax,60
                              0x0f, 0x05 };                              // syscall
#else
#error unsupported arch
#endif

static void put16(uint8_t *p, uint16_t v) { memcpy(p, &v, 2); }
static void put32(uint8_t *p, uint32_t v) { memcpy(p, &v, 4); }
static void put64(uint8_t *p, uint64_t v) { memcpy(p, &v, 8); }

int main(void) {
    static uint8_t img[0x4000];
    const uint8_t *code[NSEG] = { C0, C1, C2 };
    const size_t clen[NSEG] = { sizeof C0, sizeof C1, sizeof C2 };
    memcpy(img, "\x7f" "ELF\2\1\1", 7);
    put16(img + 16, 2);                 // ET_EXEC
    put16(img + 18, EM);
    put32(img + 20, 1);
    put64(img + 24, VA[0]);             // entry
    put64(img + 32, 64);                // phoff
    put16(img + 52, 64);
    put16(img + 54, 56);
    put16(img + 56, NSEG);
    put16(img + 58, 64);
    for (int i = 0; i < NSEG; i++) {
        uint8_t *ph = img + 64 + 56 * i;
        put32(ph + 0, 1);               // PT_LOAD
        put32(ph + 4, 5);               // R+X
        put64(ph + 8, 0x1000 * (i + 1));          // offset
        put64(ph + 16, VA[i]);
        put64(ph + 24, VA[i]);
        put64(ph + 32, 0x1000);
        put64(ph + 40, 0x1000);
        put64(ph + 48, 0x1000);
        memcpy(img + 0x1000 * (i + 1), code[i], clen[i]);
    }
    const char *path = "/root/.execdev.elf";
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (fd < 0 || write(fd, img, sizeof img) != (ssize_t)sizeof img) { perror("write"); return 1; }
    close(fd);
    pid_t pid = fork();
    if (pid == 0) { execl(path, path, (char *)0); _exit(127); }
    int st = 0;
    waitpid(pid, &st, 0);
    unlink(path);
    if (WIFEXITED(st) && WEXITSTATUS(st) == 42) { puts("execdev: PASS"); return 0; }
    printf("execdev: FAIL status=0x%x\n", st);
    return 1;
}
