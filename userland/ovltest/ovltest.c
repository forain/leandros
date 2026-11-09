/*
 * ovltest -- overlayfs groundwork and (later) overlay semantics. Builds for
 * LeandrOS (zig cc -target <arch>-linux-musl -static) and for a Linux host.
 *
 * Group O (lane ovlmknod): mknod(2) for chr/blk/fifo/sock nodes on f2fs and
 * tmpfs. Whiteouts (char 0:0) are open to everyone, other devices need
 * CAP_MKNOD, st_rdev / d_type / open() behave as on Linux.
 * Run as root; the unprivileged cases fork and drop to uid 1000.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int npass, nfail;
static char fsname[16];

static void check(const char *name, int ok)
{
    printf("%s %s [%s]\n", ok ? "PASS" : "FAIL", name, fsname);
    if (ok) npass++; else nfail++;
}

static int dtype_of(const char *dir, const char *name)
{
    DIR *d = opendir(dir);
    struct dirent *e;
    int t = -1;
    if (!d) return -2;
    while ((e = readdir(d)))
        if (!strcmp(e->d_name, name)) { t = e->d_type; break; }
    closedir(d);
    return t;
}

/* Run fn as uid 1000 in a child; returns its exit code. */
static int as_user(int (*fn)(const char *), const char *base)
{
    pid_t p = fork();
    int st;
    if (p == 0) {
        if (setgid(1000) || setuid(1000)) _exit(90);
        _exit(fn(base));
    }
    if (p < 0 || waitpid(p, &st, 0) < 0 || !WIFEXITED(st)) return -1;
    return WEXITSTATUS(st);
}

/* Child: bit i of the exit code = case i passed. */
static int user_cases(const char *base)
{
    char p[256];
    struct stat st;
    int r = 0;
    snprintf(p, sizeof p, "%s/uwh", base);
    if (mknod(p, S_IFCHR | 0600, makedev(0, 0)) == 0) r |= 1;
    if (lstat(p, &st) == 0 && S_ISCHR(st.st_mode) && st.st_rdev == 0 && st.st_uid == 1000) r |= 2;
    snprintf(p, sizeof p, "%s/uchr", base);
    if (mknod(p, S_IFCHR | 0600, makedev(1, 3)) < 0 && errno == EPERM) r |= 4;
    snprintf(p, sizeof p, "%s/ublk", base);
    if (mknod(p, S_IFBLK | 0600, makedev(7, 0)) < 0 && errno == EPERM) r |= 8;
    snprintf(p, sizeof p, "%s/ufifo", base);
    if (mknod(p, S_IFIFO | 0600, 0) == 0 && lstat(p, &st) == 0 && S_ISFIFO(st.st_mode)) r |= 16;
    snprintf(p, sizeof p, "%s/usock", base);
    if (mknod(p, S_IFSOCK | 0600, 0) == 0 && lstat(p, &st) == 0 && S_ISSOCK(st.st_mode)) r |= 32;
    snprintf(p, sizeof p, "%s/udir", base);
    if (mknod(p, S_IFDIR | 0700, 0) < 0 && errno == EPERM) r |= 64;
    return r;
}

static void run_group(const char *base0, const char *label)
{
    char base[256], p[300], q[300];
    struct stat st;
    int fd, r;

    snprintf(fsname, sizeof fsname, "%s", label);
    snprintf(base, sizeof base, "%s/ovltest.%d", base0, (int)getpid());
    mkdir(base, 0777);
    chmod(base, 01777);

    /* root: whiteout */
    snprintf(p, sizeof p, "%s/wh", base);
    check("O1 root mknod whiteout 0:0", mknod(p, S_IFCHR | 0600, 0) == 0);
    check("O2 whiteout is S_IFCHR rdev 0",
          lstat(p, &st) == 0 && S_ISCHR(st.st_mode) && st.st_rdev == 0 && (st.st_mode & 07777) == 0600);
    errno = 0;
    check("O3 open whiteout -> ENXIO", open(p, O_RDONLY) < 0 && errno == ENXIO);
    check("O4 getdents d_type DT_CHR", dtype_of(base, "wh") == DT_CHR);
    check("O5 mknod over existing -> EEXIST",
          mknod(p, S_IFCHR | 0600, 0) < 0 && errno == EEXIST);

    /* unprivileged */
    r = as_user(user_cases, base);
    check("O6 user whiteout allowed", r >= 0 && (r & 1));
    check("O7 user whiteout stat chr rdev0 owner", r >= 0 && (r & 2));
    check("O8 user chr 1:3 -> EPERM", r >= 0 && (r & 4));
    check("O9 user blk -> EPERM", r >= 0 && (r & 8));
    check("O10 user fifo allowed", r >= 0 && (r & 16));
    check("O11 user socket allowed", r >= 0 && (r & 32));
    check("O12 mknod S_IFDIR -> EPERM", r >= 0 && (r & 64));

    /* root: real device nodes reach the driver */
    snprintf(p, sizeof p, "%s/null", base);
    check("O13 root mknod chr 1:3", mknod(p, S_IFCHR | 0666, makedev(1, 3)) == 0);
    check("O14 chr stat rdev 1:3",
          stat(p, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 1 && minor(st.st_rdev) == 3);
    fd = open(p, O_RDWR);
    check("O15 open chr 1:3 acts as /dev/null",
          fd >= 0 && write(fd, "abc", 3) == 3 && read(fd, q, 8) == 0);
    if (fd >= 0) close(fd);
    snprintf(p, sizeof p, "%s/zero", base);
    mknod(p, S_IFCHR | 0666, makedev(1, 5));
    fd = open(p, O_RDONLY);
    memset(q, 0xff, 8);
    check("O16 open chr 1:5 acts as /dev/zero", fd >= 0 && read(fd, q, 4) == 4 && !q[0] && !q[3]);
    if (fd >= 0) close(fd);
    snprintf(p, sizeof p, "%s/big", base);
    check("O17 big dev_t mknod+stat roundtrip",
          mknod(p, S_IFCHR | 0600, makedev(300, 70000)) == 0 && lstat(p, &st) == 0
          && major(st.st_rdev) == 300 && minor(st.st_rdev) == 70000);
    errno = 0;
    check("O18 open chr unknown major -> ENXIO", open(p, O_RDONLY) < 0 && errno == ENXIO);
    snprintf(p, sizeof p, "%s/blk", base);
    check("O19 root mknod blk 7:0", mknod(p, S_IFBLK | 0660, makedev(7, 0)) == 0);
    check("O20 blk stat S_IFBLK rdev 7:0 (stat)",
          lstat(p, &st) == 0 && S_ISBLK(st.st_mode) && major(st.st_rdev) == 7 && minor(st.st_rdev) == 0);
    snprintf(p, sizeof p, "%s/fifo", base);
    check("O21 root mknod fifo", mknod(p, S_IFIFO | 0644, 0) == 0 && lstat(p, &st) == 0 && S_ISFIFO(st.st_mode));
    snprintf(p, sizeof p, "%s/sock", base);
    check("O22 root mknod socket", mknod(p, S_IFSOCK | 0644, 0) == 0 && lstat(p, &st) == 0 && S_ISSOCK(st.st_mode));
    check("O23 getdents d_type BLK/FIFO/SOCK",
          dtype_of(base, "blk") == DT_BLK && dtype_of(base, "fifo") == DT_FIFO
          && dtype_of(base, "sock") == DT_SOCK && dtype_of(base, "null") == DT_CHR);
    {
        struct sockaddr_un a;
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        memset(&a, 0, sizeof a);
        a.sun_family = AF_UNIX;
        snprintf(a.sun_path, sizeof a.sun_path, "%s", p);
        errno = 0;
        /* AF_UNIX connect only resolves tmpfs nodes (see unix_resolve_node):
         * on f2fs a listener-less node reads as ENOENT, not ECONNREFUSED. */
        check("O24 connect to listener-less socket node fails (ECONNREFUSED)",
              connect(s, (struct sockaddr *)&a, sizeof a) < 0
              && (errno == ECONNREFUSED || (errno == ENOENT && !strcmp(fsname, "f2fs"))));
        close(s);
    }

    /* unlink / rename */
    snprintf(p, sizeof p, "%s/wh", base);
    snprintf(q, sizeof q, "%s/file", base);
    fd = open(q, O_CREAT | O_WRONLY, 0644);
    if (fd >= 0) { write(fd, "x", 1); close(fd); }
    check("O25 rename whiteout over regular file",
          rename(p, q) == 0 && lstat(q, &st) == 0 && S_ISCHR(st.st_mode) && st.st_rdev == 0
          && lstat(p, &st) < 0);
    check("O26 unlink whiteout", unlink(q) == 0 && lstat(q, &st) < 0 && errno == ENOENT);
    snprintf(p, sizeof p, "%s/null", base);
    snprintf(q, sizeof q, "%s/null2", base);
    check("O27 rename keeps chr rdev",
          rename(p, q) == 0 && lstat(q, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 1 && minor(st.st_rdev) == 3);
    check("O28 link keeps chr rdev",
          (snprintf(p, sizeof p, "%s/nulllink", base), link(q, p)) == 0
          && lstat(p, &st) == 0 && S_ISCHR(st.st_mode) && major(st.st_rdev) == 1 && st.st_nlink == 2);

    /* cleanup */
    {
        const char *names[] = { "nulllink", "null2", "zero", "big", "blk", "fifo", "sock",
                                "uwh", "ufifo", "usock", "wh", "file" };
        unsigned i;
        for (i = 0; i < sizeof names / sizeof *names; i++) {
            snprintf(p, sizeof p, "%s/%s", base, names[i]);
            unlink(p);
        }
        check("O29 cleanup: rmdir after unlinking every node", rmdir(base) == 0);
    }
}

int main(void)
{
    run_group("/home", "f2fs");
    run_group("/tmp", "tmpfs");
    printf("ovltest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
