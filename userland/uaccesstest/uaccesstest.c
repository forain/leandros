/*
 * uaccesstest — the kernel copies into (and out of) user pages the program
 * has never touched. A demand-zero .bss/anonymous page is a valid buffer:
 * read(2) must fault it in, not answer EFAULT (lane ttyefault: the console
 * read path returned -14 for `static char b[4096]; read(0, b, 4096)`).
 *
 * Every case uses its own page-aligned static buffer that nothing touches
 * before the syscall under test.
 *
 *   console_read   write a CPR query (ESC[6n) to the console, read the
 *                  kernel's reply from fd 0 (skipped when fd 0/1 is not a tty)
 *   pipe_read      read(2) from a pipe
 *   pipe_readv     readv(2) from a pipe, two iovecs on untouched pages
 *   pipe_write     write(2) FROM an untouched page (must read zeros)
 *   file_read      read(2) from a regular file in /tmp
 *   sock_read      read(2) from an AF_UNIX socketpair
 *   pty_read       read(2) from a pty slave
 *   anon_read      read(2) into a fresh MAP_ANONYMOUS page
 *
 * Builds for the host too: cc uaccesstest.c
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#define PG 4096
#define BUF(n) static char n[2 * PG] __attribute__((aligned(PG)))
BUF(b_console); BUF(b_pipe); BUF(b_readv1); BUF(b_readv2); BUF(b_write);
BUF(b_file); BUF(b_sock); BUF(b_pty);

static int fails;
static void result(const char *name, int ok, const char *why) {
    printf("%s: %s%s%s\n", name, ok ? "PASS" : "FAIL", why ? " - " : "", why ? why : "");
    if (!ok) fails++;
}
static void check_n(const char *name, ssize_t r, ssize_t want, const char *got, const char *exp) {
    char why[96];
    if (r < 0) { snprintf(why, sizeof why, "returned -1 errno %d (%s)", errno, strerror(errno)); result(name, 0, why); return; }
    if (r != want) { snprintf(why, sizeof why, "returned %zd, want %zd", r, want); result(name, 0, why); return; }
    if (exp && memcmp(got, exp, want) != 0) { result(name, 0, "wrong bytes"); return; }
    result(name, 1, NULL);
}

static void console_read(void) {
    if (!isatty(0) || !isatty(1)) { printf("console_read: SKIP - stdio is not a tty\n"); return; }
    if (write(1, "\x1b[6n", 4) != 4) { result("console_read", 0, "CPR write failed"); return; }
    ssize_t r = read(0, b_console, PG);
    char why[96];
    if (r < 0) { snprintf(why, sizeof why, "returned -1 errno %d (%s)", errno, strerror(errno)); result("console_read", 0, why); return; }
    result("console_read", r >= 6 && b_console[0] == 0x1b && b_console[r - 1] == 'R', r >= 6 ? NULL : "short reply");
}

static void pipe_cases(void) {
    int p[2];
    if (pipe(p) < 0) { result("pipe_read", 0, "pipe"); return; }
    write(p[1], "hello", 5);
    check_n("pipe_read", read(p[0], b_pipe, PG), 5, b_pipe, "hello");

    write(p[1], "abcdefgh", 8);
    struct iovec iov[2] = { { b_readv1, 3 }, { b_readv2, PG } };
    ssize_t r = readv(p[0], iov, 2);
    check_n("pipe_readv", r, 8, b_readv1, "abc");
    if (r == 8 && memcmp(b_readv2, "defgh", 5) != 0) result("pipe_readv_tail", 0, "wrong bytes");

    r = write(p[1], b_write, PG);
    static char back[PG];
    if (r == PG) {
        ssize_t got = 0;
        while (got < PG) { ssize_t k = read(p[0], back + got, PG - got); if (k <= 0) break; got += k; }
        static const char zero[PG];
        check_n("pipe_write", got, PG, back, zero);
    } else check_n("pipe_write", r, PG, NULL, NULL);
    close(p[0]); close(p[1]);
}

static void file_read(void) {
    const char *path = "/tmp/uaccesstest.dat";
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) { result("file_read", 0, "open"); return; }
    write(fd, "filedata", 8);
    lseek(fd, 0, SEEK_SET);
    check_n("file_read", read(fd, b_file, PG), 8, b_file, "filedata");
    close(fd); unlink(path);
}

static void sock_read(void) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { result("sock_read", 0, "socketpair"); return; }
    write(sv[1], "sockdata", 8);
    check_n("sock_read", read(sv[0], b_sock, PG), 8, b_sock, "sockdata");
    close(sv[0]); close(sv[1]);
}

static void pty_read(void) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    if (m < 0 || grantpt(m) < 0 || unlockpt(m) < 0) { printf("pty_read: SKIP - no ptmx\n"); return; }
    int s = open(ptsname(m), O_RDWR | O_NOCTTY);
    if (s < 0) { result("pty_read", 0, "open slave"); close(m); return; }
    write(m, "ptyline\n", 8);
    ssize_t r = read(s, b_pty, PG);
    if (r > 0 && b_pty[r - 1] == '\n') r = 8; /* "ptyline\n" (icrnl may not apply) */
    check_n("pty_read", r, 8, b_pty, "ptyline");
    close(s); close(m);
}

static void anon_read(void) {
    char *a = mmap(NULL, 2 * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (a == MAP_FAILED) { result("anon_read", 0, "mmap"); return; }
    int p[2];
    pipe(p);
    write(p[1], "anon", 4);
    check_n("anon_read", read(p[0], a + PG - 2, PG), 4, a + PG - 2, "anon");
    close(p[0]); close(p[1]); munmap(a, 2 * PG);
}

int main(int argc, char **argv) {
    int only_console = argc > 1 && strcmp(argv[1], "console") == 0;
    console_read();
    if (!only_console) { pipe_cases(); file_read(); sock_read(); pty_read(); anon_read(); }
    printf("uaccesstest: %s (%d failed)\n", fails ? "FAIL" : "PASS", fails);
    return fails ? 1 : 0;
}
