// memfdtest: the sequence crun's cloned-binary protection performs at startup.
// Prints "<step>: PASS" / "<step>: FAIL (errno N)" and "memfdtest: ALL PASS".
// With argument "child" it is the re-exec'd image: it checks /proc/self/exe
// names a memfd and the seals, then exits 0 ("MEMFD-CHILD-OK").
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#define F_GET_SEALS 1034
#endif
#define SEAL_SEAL 1
#define SEAL_SHRINK 2
#define SEAL_GROW 4
#define SEAL_WRITE 8
#define ALL_SEALS (SEAL_SEAL | SEAL_SHRINK | SEAL_GROW | SEAL_WRITE)

static int fails;
static void ok(const char *s, int cond)
{
    if (cond) printf("%s: PASS\n", s);
    else { printf("%s: FAIL (errno %d)\n", s, errno); fails++; }
    fflush(stdout);
}

static int child(void)
{
    char buf[256] = {0};
    ssize_t n = readlink("/proc/self/exe", buf, sizeof buf - 1);
    printf("child /proc/self/exe -> %s\n", n > 0 ? buf : "(readlink failed)");
    int good = n > 0 && strncmp(buf, "/memfd:", 7) == 0;
    int fd = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    int seals = fd >= 0 ? fcntl(fd, F_GET_SEALS) : -1;
    printf("child seals=%d\n", seals);
    if (good && seals == ALL_SEALS) { printf("MEMFD-CHILD-OK\n"); return 0; }
    printf("MEMFD-CHILD-BAD\n");
    return 1;
}

int main(int argc, char **argv, char **envp)
{
    if (argc > 1 && !strcmp(argv[1], "child")) return child();

    // 1: not already sealed
    int self = open("/proc/self/exe", O_RDONLY | O_CLOEXEC);
    ok("open_self_exe", self >= 0);
    errno = 0;
    int s0 = fcntl(self, F_GET_SEALS);
    ok("get_seals_on_regular_is_not_all", s0 != ALL_SEALS);

    // 2: memfd_create
    int m = syscall(SYS_memfd_create, "crun", 1 /*CLOEXEC*/ | 2 /*ALLOW_SEALING*/);
    ok("memfd_create", m >= 0);
    errno = 0;
    ok("get_seals_fresh_is_0", fcntl(m, F_GET_SEALS) == 0);

    // 3: copy self into it
    struct stat st;
    ok("fstat_self", fstat(self, &st) == 0);
    off_t off = 0; ssize_t tot = 0, n;
    while (tot < st.st_size && (n = sendfile(m, self, &off, st.st_size - tot)) > 0) tot += n;
    ok("sendfile_regular_to_memfd", tot == st.st_size);
    struct stat ms;
    ok("fstat_memfd_size", fstat(m, &ms) == 0 && ms.st_size == st.st_size);
    lseek(self, 0, SEEK_SET);
    off_t o2 = 0; ssize_t cf = copy_file_range(self, &o2, m, NULL, 0, 0);
    // size 0 copy is a noop, just must not fail with ENOSYS-ish weirdness
    ok("copy_file_range_zero", cf == 0);
    char a[64], b[64];
    ok("pread_matches", pread(self, a, 64, 0) == 64 && pread(m, b, 64, 0) == 64 && !memcmp(a, b, 64));

    // 4: seal
    ok("add_seals", fcntl(m, F_ADD_SEALS, ALL_SEALS) == 0);
    ok("get_seals_all", fcntl(m, F_GET_SEALS) == ALL_SEALS);
    errno = 0;
    int r = fcntl(m, F_ADD_SEALS, SEAL_WRITE);
    ok("add_seals_after_seal_seal_EPERM", r < 0 && errno == EPERM);
    errno = 0;
    r = write(m, "x", 1);
    ok("write_sealed_EPERM", r < 0 && errno == EPERM);
    errno = 0;
    r = ftruncate(m, 1);
    ok("ftruncate_sealed_EPERM", r < 0 && errno == EPERM);
    errno = 0;
    r = ftruncate(m, ms.st_size + 4096);
    ok("grow_sealed_EPERM", r < 0 && errno == EPERM);

    // 5: fexecve
    pid_t p = fork();
    if (p == 0) {
        char *av[] = {"memfdtest", "child", NULL};
        syscall(SYS_execveat, m, "", av, envp, AT_EMPTY_PATH);
        printf("execveat memfd: errno %d\n", errno);
        fflush(stdout);
        _exit(111);
    }
    int stt = 0;
    waitpid(p, &stt, 0);
    errno = WIFEXITED(stt) ? WEXITSTATUS(stt) : -1;
    ok("fexecve_memfd_child_ok", WIFEXITED(stt) && WEXITSTATUS(stt) == 0);

    // 6: exec via /proc/self/fd/N
    char path[64];
    snprintf(path, sizeof path, "/proc/self/fd/%d", m);
    p = fork();
    if (p == 0) {
        char *av[] = {"memfdtest", "child", NULL};
        execve(path, av, envp);
        printf("execve %s: errno %d\n", path, errno);
        fflush(stdout);
        _exit(111);
    }
    waitpid(p, &stt, 0);
    errno = WIFEXITED(stt) ? WEXITSTATUS(stt) : -1;
    ok("exec_proc_self_fd_child_ok", WIFEXITED(stt) && WEXITSTATUS(stt) == 0);

    printf(fails ? "memfdtest: %d FAILED\n" : "memfdtest: ALL PASS\n", fails);
    return fails != 0;
}
