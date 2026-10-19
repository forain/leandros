// kexec: a small kexec-tools compatible front end over kexec_file_load(2)
// and reboot(LINUX_REBOOT_CMD_KEXEC).
//
//   kexec -l KERNEL [--initrd=FILE] [--append=STR | --command-line=STR]
//                   [--reuse-cmdline]     load KERNEL as the next kernel
//   kexec -u                              unload the loaded kernel
//   kexec -e                              sync, then boot the loaded kernel
//   kexec KERNEL [options]                load, then exec (-l + -e)
//   -c CPU / --cpu=CPU (with -e)          issue reboot(KEXEC) from CPU CPU:
//                                         there is no CPU affinity, so this
//                                         sleeps until getcpu(2) reports it
//                                         (up to 30 s) - a test aid for the
//                                         hand-off from a secondary CPU
//
// --reuse-cmdline starts from /proc/cmdline; --append then adds to it. Run
// as root.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/reboot.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define KEXEC_FILE_UNLOAD 1
#define KEXEC_FILE_NO_INITRAMFS 4
#define CMD_KEXEC 0x45584543 // LINUX_REBOOT_CMD_KEXEC

static void usage(void)
{
    fputs("usage: kexec -l KERNEL [--initrd=FILE] [--append=STR] [--reuse-cmdline]\n"
          "       kexec -u\n"
          "       kexec -e [-c CPU]\n"
          "       kexec KERNEL [--initrd=FILE] [--append=STR] [--reuse-cmdline]  (load + exec)\n",
          stderr);
}

static int do_unload(void)
{
    if (syscall(SYS_kexec_file_load, -1, -1, 0, NULL, KEXEC_FILE_UNLOAD) != 0) {
        fprintf(stderr, "kexec: unload failed: %s\n", strerror(errno));
        return 1;
    }
    return 0;
}

static unsigned cur_cpu(void)
{
    unsigned c = ~0u;
    syscall(SYS_getcpu, &c, NULL, NULL);
    return c;
}

// Sleep (each wake-up is a new placement) until this thread runs on `want`.
static int reach_cpu(unsigned want)
{
    for (unsigned i = 0; i < 30000; i++) {
        if (cur_cpu() == want) return 0;
        struct timespec ts = { 0, (long)(200000 + (i * 7919u) % 1800000) };
        if (i % 3 == 2) sched_yield(); else nanosleep(&ts, NULL);
    }
    fprintf(stderr, "kexec: never ran on cpu%u (last cpu%u)\n", want, cur_cpu());
    return -1;
}

static int do_exec(int cpu)
{
    sync();
    if (cpu >= 0) {
        if (reach_cpu((unsigned)cpu) != 0) return 1;
        fprintf(stderr, "kexec: reboot(KEXEC) from cpu%u\n", cur_cpu());
    }
    reboot(CMD_KEXEC);
    if (errno == EINVAL)
        fprintf(stderr, "kexec: no kernel loaded (use kexec -l first)\n");
    else
        fprintf(stderr, "kexec: reboot(KEXEC) failed: %s\n", strerror(errno));
    return 1;
}

static int do_load(const char *kernel, const char *initrd, const char *append, int reuse)
{
    char cmd[4096] = "";
    size_t len = 0;

    if (reuse) {
        int fd = open("/proc/cmdline", O_RDONLY);
        if (fd < 0) {
            fprintf(stderr, "kexec: cannot open /proc/cmdline: %s\n", strerror(errno));
            return 1;
        }
        ssize_t n = read(fd, cmd, sizeof cmd - 1);
        close(fd);
        if (n < 0) {
            fprintf(stderr, "kexec: cannot read /proc/cmdline: %s\n", strerror(errno));
            return 1;
        }
        len = (size_t)n;
        while (len > 0 && (cmd[len - 1] == '\n' || cmd[len - 1] == ' ')) len--;
        cmd[len] = 0;
    }
    if (append) {
        if (len + strlen(append) + 2 > sizeof cmd) {
            fprintf(stderr, "kexec: command line too long\n");
            return 1;
        }
        len += snprintf(cmd + len, sizeof cmd - len, "%s%s", len ? " " : "", append);
    }

    int kfd = open(kernel, O_RDONLY);
    if (kfd < 0) {
        fprintf(stderr, "kexec: cannot open kernel %s: %s\n", kernel, strerror(errno));
        return 1;
    }
    int ifd = -1;
    if (initrd) {
        ifd = open(initrd, O_RDONLY);
        if (ifd < 0) {
            fprintf(stderr, "kexec: cannot open initrd %s: %s\n", initrd, strerror(errno));
            return 1;
        }
    }
    long r = syscall(SYS_kexec_file_load, kfd, ifd, (unsigned long)(len + 1), cmd,
                     (unsigned long)(initrd ? 0 : KEXEC_FILE_NO_INITRAMFS));
    if (r != 0) {
        fprintf(stderr, "kexec: kexec_file_load %s failed: %s\n", kernel, strerror(errno));
        return 1;
    }
    return 0;
}

int main(int argc, char **argv)
{
    const char *kernel = NULL, *initrd = NULL, *append = NULL;
    int load = 0, unload = 0, exec = 0, reuse = 0, cpu = -1;

    for (int i = 1; i < argc; i++) {
        char *a = argv[i];
        if (!strcmp(a, "-l") || !strcmp(a, "--load")) load = 1;
        else if (!strcmp(a, "-u") || !strcmp(a, "--unload")) unload = 1;
        else if (!strcmp(a, "-e") || !strcmp(a, "--exec")) exec = 1;
        else if (!strcmp(a, "--reuse-cmdline")) reuse = 1;
        else if (!strcmp(a, "-c") && i + 1 < argc) cpu = atoi(argv[++i]);
        else if (!strncmp(a, "--cpu=", 6)) cpu = atoi(a + 6);
        else if (!strncmp(a, "--initrd=", 9)) initrd = a + 9;
        else if (!strncmp(a, "--ramdisk=", 10)) initrd = a + 10;
        else if (!strncmp(a, "--append=", 9)) append = a + 9;
        else if (!strncmp(a, "--command-line=", 15)) append = a + 15;
        else if (!strcmp(a, "-h") || !strcmp(a, "--help")) { usage(); return 0; }
        else if (a[0] == '-') {
            fprintf(stderr, "kexec: unknown option %s\n", a);
            usage();
            return 1;
        } else if (!kernel) kernel = a;
        else {
            fprintf(stderr, "kexec: unexpected argument %s\n", a);
            usage();
            return 1;
        }
    }

    if (unload) {
        if (load || exec || kernel) { usage(); return 1; }
        return do_unload();
    }
    if (exec && !load && !kernel) return do_exec(cpu);
    if (!kernel) { usage(); return 1; }
    // `kexec KERNEL` = load + exec; `kexec -l KERNEL` = load only.
    if (do_load(kernel, initrd, append, reuse)) return 1;
    if (!load || exec) return do_exec(cpu);
    return 0;
}
