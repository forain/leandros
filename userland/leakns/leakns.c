/* leakns [N] [uid]: leak N (default 16) sleeping user-namespace holders of one uid on purpose
 * (fork, unshare(CLONE_NEWUSER), pause; the parent exits so they reparent to init).
 * Used to find what a leaked userns slot breaks (DNS/apk in pk-test). `leakns kill` reaps them. */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
int main(int argc, char **argv)
{
    if (argc > 1 && !strcmp(argv[1], "kill")) { system("pkill -KILL -u 1001 pause 2>/dev/null"); return 0; }
    int n = argc > 1 ? atoi(argv[1]) : 16;
    int uid = argc > 2 ? atoi(argv[2]) : 1001;
    int ready[2];
    if (pipe(ready)) return 2;
    pid_t v = fork();
    if (v == 0) {
        if (setgid(uid) || setuid(uid)) _exit(2);
        for (int i = 0; i < n; i++) {
            pid_t g = fork();
            if (g == 0) {
                int r = unshare(CLONE_NEWUSER);
                if (write(ready[1], r ? "f" : "k", 1) != 1) _exit(3);
                pause(); _exit(0);
            }
            if (g < 0) { printf("leakns: fork %d failed errno=%d\n", i, errno); break; }
        }
        _exit(0);
    }
    close(ready[1]);
    int ok = 0, bad = 0; char c;
    while (read(ready[0], &c, 1) == 1) { if (c == 'k') ok++; else bad++; if (ok + bad == n) break; }
    int st; waitpid(v, &st, 0);
    printf("leakns: %d holders of uid %d leaked (%d unshare failures)\n", ok, uid, bad);
    return 0;
}
