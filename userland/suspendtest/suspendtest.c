// suspendtest: one freeze-only system-sleep cycle (kernel pm, step 1 of
// suspend to RAM). reboot(2) with LeandrOS's private PM_TEST command runs
// every device's suspend hook, holds the quiesced state, and resumes them;
// no platform sleep. Checks that disk, network and display survive:
//   pm_cycle        the cycle reports "cycle complete", every hook ok
//   io_during       a child doing write+fsync through the cycle finishes clean
//   disk_after      1 MiB written, fsynced and read back after the cycle
//   net_after       e1000e back with link (or absent); --net HOST:PORT also connects
//   display_after   i915 plane scanning out again (or no KMS display)
// Usage: suspendtest [--hold MS] [--xhci-halt] [--net A.B.C.D:PORT] [--dir DIR]
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CMD_PM_TEST 0x504D5453u
struct req { uint32_t flags, hold_ms; uint64_t buf, buf_len; };
static char rep[16384];
static int fails;

static void result(const char *c, int ok, const char *why) {
    printf("%s: %s%s%s\n", c, ok ? "PASS" : "FAIL", why && *why ? " " : "", why ? why : "");
    if (!ok) fails++;
}

static int write_file(const char *p, int seed, int n) {
    char b[4096];
    int fd = open(p, O_CREAT | O_TRUNC | O_WRONLY, 0644);
    if (fd < 0) return -1;
    for (int i = 0; i < n; i++) {
        for (int j = 0; j < 4096; j++) b[j] = (char)(seed + i * 7 + j);
        if (write(fd, b, 4096) != 4096) { close(fd); return -1; }
    }
    int r = fsync(fd);
    close(fd);
    return r;
}

static int check_file(const char *p, int seed, int n) {
    char b[4096];
    int fd = open(p, O_RDONLY);
    if (fd < 0) return -1;
    for (int i = 0; i < n; i++) {
        if (read(fd, b, 4096) != 4096) { close(fd); return -1; }
        for (int j = 0; j < 4096; j++) if (b[j] != (char)(seed + i * 7 + j)) { close(fd); return -2; }
    }
    close(fd);
    return 0;
}

// --sysfs: /sys/power and /sys/class/rtc (lane s3sysfs). x86_64 has the rtc.
static int wr(const char *p, const char *v) {
    int fd = open(p, O_WRONLY); if (fd < 0) return -errno;
    int r = write(fd, v, strlen(v)); int e = errno; close(fd);
    return r < 0 ? -e : 0;
}
static int rd(const char *p, char *b, size_t n) {
    int fd = open(p, O_RDONLY); if (fd < 0) return -errno;
    int r = read(fd, b, n - 1); close(fd);
    if (r < 0) return -errno;
    b[r] = 0; return r;
}
static int sysfs_mode(void) {
    char b[128]; 
#ifdef __x86_64__
    int x86 = 1;
#else
    int x86 = 0;
#endif
    result("power_state", rd("/sys/power/state", b, sizeof b) > 0 && strstr(b, "freeze"), b);
    result("mem_refused", wr("/sys/power/state", "mem") == -EINVAL, "mem without pm.s3=1 is EINVAL");
    result("bad_state", wr("/sys/power/state", "bogus") == -EINVAL, "");
    result("freeze_needs_level", wr("/sys/power/pm_test", "none") == 0 && wr("/sys/power/state", "freeze") == -EINVAL, "");
    result("pm_test_set", wr("/sys/power/pm_test", "devices") == 0 && rd("/sys/power/pm_test", b, sizeof b) > 0 && strstr(b, "[devices]"), b);
    result("freeze_cycle", wr("/sys/power/state", "freeze") == 0, "pm_test=devices");
    result("fs_after", system("echo ok > /root/.suspendtest-sysfs && rm /root/.suspendtest-sysfs") == 0, "");
    if (x86) {
        result("alarm_set", wr("/sys/class/rtc/rtc0/wakealarm", "+120") == 0, "");
        long a = rd("/sys/class/rtc/rtc0/wakealarm", b, sizeof b) > 0 ? atol(b) : 0;
        char e[64]; long now = rd("/sys/class/rtc/rtc0/since_epoch", e, sizeof e) > 0 ? atol(e) : 0;
        result("alarm_readback", a > now + 100 && a <= now + 125, b);
        result("alarm_clear", wr("/sys/class/rtc/rtc0/wakealarm", "0") == 0 && rd("/sys/class/rtc/rtc0/wakealarm", b, sizeof b) >= 0 && atol(b) == 0, "");
    } else {
        result("no_rtc", wr("/sys/class/rtc/rtc0/wakealarm", "+10") < 0, "no rtc0 on this arch");
    }
    printf("suspendtest --sysfs: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}

int main(int argc, char **argv) {
    struct req r = { 0, 300, (uint64_t)(uintptr_t)rep, sizeof rep };
    const char *net = NULL, *dir = "/root";
    if (argc > 1 && !strcmp(argv[1], "--sysfs")) return sysfs_mode();
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--hold") && i + 1 < argc) r.hold_ms = atoi(argv[++i]);
        else if (!strcmp(argv[i], "--xhci-halt")) r.flags |= 1;
        else if (!strcmp(argv[i], "--net") && i + 1 < argc) net = argv[++i];
        else if (!strcmp(argv[i], "--dir") && i + 1 < argc) dir = argv[++i];
        else { fprintf(stderr, "usage: suspendtest [--hold MS] [--xhci-halt] [--net IP:PORT] [--dir DIR]\n"); return 2; }
    }
    char pa[256], pb[256];
    snprintf(pa, sizeof pa, "%s/.suspendtest-a", dir);
    snprintf(pb, sizeof pb, "%s/.suspendtest-b", dir);

    // Writer child: write+fsync rounds until told to stop.
    int pfd[2];
    if (pipe(pfd)) { perror("pipe"); return 1; }
    pid_t kid = fork();
    if (kid == 0) {
        close(pfd[0]);
        int rounds = 0, bad = 0;
        for (int i = 0; i < 200; i++) {
            if (write_file(pb, i, 16) || check_file(pb, i, 16)) bad++;
            rounds++;
            usleep(10000);
        }
        char m[64];
        int n = snprintf(m, sizeof m, "%d %d", rounds, bad);
        (void)!write(pfd[1], m, n);
        _exit(0);
    }
    close(pfd[1]);
    usleep(100000);

    long rc = syscall(SYS_reboot, 0xfee1deadu, 672274793u, CMD_PM_TEST, &r);
    int err = rc ? errno : 0;
    fputs(rep, stdout);
    char why[128];
    snprintf(why, sizeof why, "rc=%ld errno=%d", rc, err);
    result("pm_cycle", rc == 0 && strstr(rep, "cycle complete") != NULL, why);

    char m[64] = {0};
    int st = 0;
    ssize_t n = read(pfd[0], m, sizeof m - 1);
    waitpid(kid, &st, 0);
    int rounds = 0, bad = -1;
    if (n > 0) sscanf(m, "%d %d", &rounds, &bad);
    snprintf(why, sizeof why, "rounds=%d bad=%d", rounds, bad);
    result("io_during", rounds == 200 && bad == 0, why);
    unlink(pb);

    int d = write_file(pa, 42, 256);
    int c = d ? -9 : check_file(pa, 42, 256);
    unlink(pa);
    snprintf(why, sizeof why, "write=%d check=%d", d, c);
    result("disk_after", d == 0 && c == 0, why);

    int net_ok = strstr(rep, "resume e1000e: ok") &&
                 (strstr(rep, "link up") || strstr(rep, "nothing saved"));
    why[0] = 0;
    if (net) {
        char host[64]; int port = 0;
        if (sscanf(net, "%63[^:]:%d", host, &port) == 2) {
            struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
            inet_pton(AF_INET, host, &a.sin_addr);
            int s = socket(AF_INET, SOCK_STREAM, 0);
            int cr = connect(s, (struct sockaddr *)&a, sizeof a);
            snprintf(why, sizeof why, "connect %s = %d errno %d", net, cr, cr ? errno : 0);
            if (cr) net_ok = 0;
            close(s);
        }
    }
    result("net_after", net_ok, why);
    result("display_after", strstr(rep, "resume i915: ok") != NULL, "");
    printf("suspendtest: %s\n", fails ? "FAIL" : "PASS");
    return fails ? 1 : 0;
}
