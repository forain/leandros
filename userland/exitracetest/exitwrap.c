// exitwrap: test-only exit-command shim. conmon points its exit command here; we
// restore a log on fd 2 (conmon points it at /dev/null) and exec podman with no delay.
#include <fcntl.h>
#include <unistd.h>
#include <string.h>
int main(int argc, char **argv)
{
    size_t al = strlen(argv[0]);
    if (al >= 6 && !strcmp(argv[0] + al - 6, "conmon")) {
        // conmon mode: rewrite "--exit-command <path>" to this shim, exec the real conmon.
        char *cv[argc + 1];
        for (int i = 0; i < argc; i++) cv[i] = argv[i];
        cv[argc] = 0;
        cv[0] = "/usr/bin/conmon.real";
        for (int i = 1; i + 1 < argc; i++)
            if (!strcmp(cv[i], "--exit-command")) cv[i + 1] = "/bin/exitwrap";
        execv(cv[0], cv);
        return 127;
    }
    char *av[argc + 3];
    int n = 0;
    av[n++] = "/usr/bin/podman.real";
    av[n++] = "--log-level=debug";
    for (int i = 1; i < argc; i++) av[n++] = argv[i];
    av[n] = 0;
    int fd = open("/tmp/cleanup.log", O_WRONLY | O_CREAT | O_APPEND, 0644);
    if (fd >= 0) { dup2(fd, 2); dup2(fd, 1); }
    execv(av[0], av);
    return 127;
}
