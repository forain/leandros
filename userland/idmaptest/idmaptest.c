/*
 * idmaptest -- user-namespace id maps. Static zig cc, nstest style:
 * "case: PASS|FAIL|SKIP". Cases are append-only separate functions.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int npass, nfail;

static void report(const char *name, int ok)
{
    printf("%s: %s\n", name, ok ? "PASS" : "FAIL");
    if (ok) npass++; else nfail++;
}

static int slurp(const char *path, char *buf, int cap)
{
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int n = read(fd, buf, cap - 1);
    close(fd);
    if (n < 0) return -1;
    buf[n] = 0;
    return n;
}

static int writable(const char *path)
{
    int fd = open(path, O_WRONLY);
    if (fd >= 0) { close(fd); return 1; }
    return 0;
}

static void subuid_present(void)
{
    const char *files[2] = { "/etc/subuid", "/etc/subgid" };
    int ok = 1;
    for (int i = 0; i < 2; i++) {
        char b[128];
        struct stat st;
        if (slurp(files[i], b, sizeof b) < 0 || strcmp(b, "leandro:100000:65536\n") != 0) ok = 0;
        else if (stat(files[i], &st) != 0 || st.st_uid != 0 || st.st_gid != 0 || (st.st_mode & 07777) != 0644) ok = 0;
    }
    report("subuid_present", ok);
}

static int expect(const char *path, const char *want)
{
    char b[32];
    return slurp(path, b, sizeof b) >= 0 && strcmp(b, want) == 0 && !writable(path);
}

static void procsys_overflow(void)
{
    int ok = expect("/proc/sys/kernel/overflowuid", "65534\n")
          && expect("/proc/sys/kernel/overflowgid", "65534\n")
          && expect("/proc/sys/user/max_user_namespaces", "255\n");
    report("procsys_overflow", ok);
}

int main(void)
{
    subuid_present();
    procsys_overflow();
    printf("idmaptest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
