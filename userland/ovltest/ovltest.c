/*
 * ovltest - overlayfs groundwork (lane ovlreal).
 *
 *  1. /proc/sys/fs/ovl_selftest runs the kernel's real-op helpers
 *     (servers/vfs/src/ovl/real.rs) as per-mount pseudo-pids with a captured
 *     credential, on tmpfs (/tmp) and f2fs (/home): data, stat, symlink,
 *     chmod/lchown, getdents, xattr round trip, ENODATA audit, and the
 *     credential override (an unprivileged "mounter" gets EACCES / owns what
 *     it creates). The kernel replies "RESULT pass=N fail=M" plus FAIL lines.
 *  2. The user-visible xattr ENODATA audit: get/list/remove of an absent
 *     user.* name on files and directories, every path/l/f variant, on tmpfs
 *     and f2fs. Overlay's storage layer maps ONLY ENODATA to "absent".
 *
 * Runs as root. Builds for LeandrOS with zig cc -target <arch>-linux-musl.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/xattr.h>
#include <unistd.h>

static int npass, nfail;

static void check(const char *name, int ok)
{
    printf("%s %s\n", ok ? "PASS" : "FAIL", name);
    if (ok) npass++; else nfail++;
}

static void kernel_selftest(void)
{
    char buf[4096];
    int fd = open("/proc/sys/fs/ovl_selftest", O_RDONLY);
    if (fd < 0) { check("selftest_open", 0); return; }
    ssize_t n = read(fd, buf, sizeof buf - 1);
    close(fd);
    if (n <= 0) { check("selftest_read", 0); return; }
    buf[n] = 0;
    char *res = strstr(buf, "RESULT pass=");
    int pass = 0, fail = -1;
    if (res) sscanf(res, "RESULT pass=%d fail=%d", &pass, &fail);
    if (fail != 0) fputs(buf, stdout);
    printf("  (kernel selftest: %d passed)\n", pass);
    check("kernel_selftest_no_failures", res && fail == 0 && pass >= 60);
}

#define NAME_N 96

static void audit(const char *base, const char *tag)
{
    char dir[128], file[160], c[NAME_N], lbuf[256], vbuf[64];
    snprintf(dir, sizeof dir, "%s/.ovltest", base);
    snprintf(file, sizeof file, "%s/f", dir);
    unlink(file);
    rmdir(dir);
    if (mkdir(dir, 0755) != 0) { snprintf(c, sizeof c, "%s_mkdir", tag); check(c, 0); return; }
    int fd = open(file, O_RDWR | O_CREAT, 0644);
    int dfd = open(dir, O_RDONLY | O_DIRECTORY);
    if (fd < 0 || dfd < 0) { snprintf(c, sizeof c, "%s_open", tag); check(c, 0); return; }

    const char *paths[2] = { dir, file };
    const char *kind[2] = { "dir", "file" };
    int fds[2] = { dfd, fd };
    for (int i = 0; i < 2; i++) {
        const char *p = paths[i];
        int f = fds[i];
#define T(what, expr) do { snprintf(c, sizeof c, "%s_%s_%s", tag, kind[i], what); \
                           errno = 0; check(c, (expr)); } while (0)
        T("getxattr_absent",   getxattr(p, "user.absent", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
        T("lgetxattr_absent",  lgetxattr(p, "user.absent", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
        T("fgetxattr_absent",  fgetxattr(f, "user.absent", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
        T("getxattr_probe",    getxattr(p, "user.absent", NULL, 0) == -1 && errno == ENODATA);
        T("removexattr_absent",  removexattr(p, "user.absent") == -1 && errno == ENODATA);
        T("lremovexattr_absent", lremovexattr(p, "user.absent") == -1 && errno == ENODATA);
        T("fremovexattr_absent", fremovexattr(f, "user.absent") == -1 && errno == ENODATA);
        T("listxattr_empty",   listxattr(p, lbuf, sizeof lbuf) == 0);
        T("llistxattr_empty",  llistxattr(p, lbuf, sizeof lbuf) == 0);
        T("flistxattr_empty",  flistxattr(f, lbuf, sizeof lbuf) == 0);
        T("listxattr_size0",   listxattr(p, NULL, 0) == 0);
        /* round trip, then the name is absent again */
        T("setxattr",          setxattr(p, "user.k", "v", 1, 0) == 0);
        T("getxattr_present",  getxattr(p, "user.k", vbuf, sizeof vbuf) == 1 && vbuf[0] == 'v');
        T("getxattr_other_absent", getxattr(p, "user.k2", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
        T("listxattr_has_name", listxattr(p, lbuf, sizeof lbuf) == 7 && !memcmp(lbuf, "user.k\0", 7));
        T("removexattr_present", removexattr(p, "user.k") == 0);
        T("getxattr_after_remove", getxattr(p, "user.k", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
        T("removexattr_twice", removexattr(p, "user.k") == -1 && errno == ENODATA);
        T("listxattr_after",   listxattr(p, lbuf, sizeof lbuf) == 0);
        /* absent name in a non-user namespace is also "absent", not "unsupported" */
        T("security_absent",   getxattr(p, "security.ovltest", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
        T("trusted_overlay_absent", getxattr(p, "trusted.overlay.opaque", vbuf, sizeof vbuf) == -1 && errno == ENODATA);
#undef T
    }
    close(fd);
    close(dfd);
    unlink(file);
    rmdir(dir);
}

int main(void)
{
    if (geteuid() != 0) { printf("SKIP: not root\n"); return 0; }
    kernel_selftest();
    audit("/tmp", "tmpfs");
    audit("/home", "f2fs");
    printf("ovltest: %d passed, %d failed\n", npass, nfail);
    return nfail != 0;
}
