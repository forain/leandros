// kmodctltest: module syscalls from userland (closes the L3d gate 6).
// As root: finit_module of the already-loaded kmodtest -> EEXIST, delete_module
// -> EBUSY, kmodbad -> the loader's error; as uid 1000: finit_module -> EPERM.
// SKIPs when /lib/modules is absent. Prints "kmodctltest: ALL PASS" or FAIL lines.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;

static void check(const char *what, long got, long want)
{
    if (got == want) printf("PASS %s (%ld)\n", what, got);
    else { printf("FAIL %s: got %ld want %ld\n", what, got, want); fails++; }
}

static long finit(const char *path)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return -errno;
    long r = syscall(SYS_finit_module, fd, "", 0);
    int e = errno;
    close(fd);
    return r < 0 ? -e : r;
}

static int in_proc_modules(const char *name)
{
    FILE *f = fopen("/proc/modules", "r");
    char line[256];
    int found = 0;
    size_t n = strlen(name);
    if (!f) return 0;
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, name, n) && line[n] == ' ') found = 1;
    fclose(f);
    return found;
}

int main(void)
{
    DIR *d = opendir("/lib/modules");
    if (!d) { printf("kmodctltest: SKIP (no /lib/modules)\n"); return 0; }
    char good[8][320]; char bad[8][320];
    int ng = 0;
    struct dirent *de;
    while ((de = readdir(d)) && ng < 8) {
        if (de->d_name[0] == '.') continue;
        char p[320];
        snprintf(p, sizeof p, "/lib/modules/%s/kmodtest.kmod", de->d_name);
        if (access(p, R_OK) != 0) continue;
        strcpy(good[ng], p);
        snprintf(bad[ng], sizeof bad[ng], "/lib/modules/%s/kmodbad.kmod", de->d_name);
        ng++;
    }
    closedir(d);
    if (!ng) { printf("kmodctltest: SKIP (no kmodtest.kmod staged)\n"); return 0; }

    if (!in_proc_modules("kmodtest")) {
        printf("kmodctltest: SKIP (kmodtest not autoloaded)\n");
        return 0;
    }
    if (geteuid() != 0) { printf("FAIL must run as root\n"); return 1; }

    // Double load: exactly one directory matches this kernel's build id (EEXIST);
    // the other variant's image is rejected by the loader (record, not asserted).
    int eexist = 0, mine = -1;
    for (int i = 0; i < ng; i++) {
        long r = finit(good[i]);
        printf("INFO load %s -> %ld\n", good[i], r);
        if (r == -EEXIST) { eexist++; mine = i; }
    }
    check("double load gives EEXIST in exactly one build-id dir", eexist, 1);

    long u = syscall(SYS_delete_module, "kmodtest", 0);
    check("unload kmodtest refused (EBUSY)", u < 0 ? -errno : u, -EBUSY);
    check("kmodtest still listed", in_proc_modules("kmodtest"), 1);

    // Only this kernel's own directory is safe: the other directory's kmodbad
    // carries THIS kernel's build id but is linked against the other variant's
    // kapi addresses, so a load would run it.
    if (mine >= 0 && access(bad[mine], R_OK) == 0) {
        long r = finit(bad[mine]);
        printf("INFO load %s -> %ld (loader error, recorded)\n", bad[mine], r);
        if (r >= 0) { printf("FAIL kmodbad loaded\n"); fails++; }
        else check("kmodbad refused with a loader errno", r, -ENOEXEC);
    }

    pid_t pid = fork();
    if (pid == 0) {
        if (setgid(1000) || setuid(1000)) _exit(90);
        long r = finit(good[0]);
        long du = syscall(SYS_delete_module, "kmodtest", 0);
        du = du < 0 ? -errno : du;
        _exit(r == -EPERM && du == -EPERM ? 0 : 1);
    }
    int st = 0;
    waitpid(pid, &st, 0);
    check("non-root load/unload gives EPERM", WIFEXITED(st) ? WEXITSTATUS(st) : 99, 0);

    if (fails) { printf("kmodctltest: %d FAILED\n", fails); return 1; }
    printf("kmodctltest: ALL PASS\n");
    return 0;
}
