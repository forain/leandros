// kexectest: kexec_file_load load/replace/unload against /boot/kexec (the
// kernel-direct ELF staged into the image). Does not jump; the next lane does.
// Run as root. Prints PASS/FAIL per check and "kexectest: ALL PASS".
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define KEXEC_FILE_UNLOAD 1
#define KEXEC_FILE_ON_CRASH 2
#define KEXEC_FILE_NO_INITRAMFS 4

static int fails;

static long kfl(int kfd, int ifd, unsigned long len, const char *cmd, unsigned long flags)
{
    long r = syscall(SYS_kexec_file_load, kfd, ifd, len, cmd, flags);
    return r < 0 ? -errno : r;
}

static void check(const char *name, long got, long want)
{
    if (got == want) {
        printf("PASS %s\n", name);
    } else {
        printf("FAIL %s: got %ld want %ld\n", name, got, want);
        fails++;
    }
}

// A load can legitimately be refused with EADDRNOTAVAIL on x86_64 under OVMF:
// the image's fixed 0x100000 destination crosses firmware ACPI NVS memory.
// That refusal is the correct safe answer; report it as SKIP, not FAIL.
static void check_load(const char *name, long got)
{
#ifdef __x86_64__
    if (got == -EADDRNOTAVAIL) {
        printf("SKIP %s: destination not usable RAM on this machine\n", name);
        return;
    }
#endif
    check(name, got, 0);
}

int main(void)
{
    static const char cmd[] = "root=/dev/vda console=ttyAMA0 kexectest=1";
    int kfd = open("/boot/kexec", O_RDONLY);
    if (kfd < 0) {
        printf("FAIL open /boot/kexec: %s\n", strerror(errno));
        return 1;
    }
    int tfd = open("/etc/passwd", O_RDONLY);
    int ifd = open("/bin/hello", O_RDONLY); // any file works as an initrd blob

    check_load("load", kfl(kfd, -1, sizeof cmd, cmd, KEXEC_FILE_NO_INITRAMFS));
    check_load("reload (replaces)", kfl(kfd, -1, 0, NULL, KEXEC_FILE_NO_INITRAMFS));
    if (ifd >= 0)
        check_load("load with initrd", kfl(kfd, ifd, sizeof cmd, cmd, 0));
    check("unload", kfl(-1, -1, 0, NULL, KEXEC_FILE_UNLOAD), 0);
    check("unload again", kfl(-1, -1, 0, NULL, KEXEC_FILE_UNLOAD), 0);
    check("bad flags (ON_CRASH)", kfl(kfd, -1, 0, NULL, KEXEC_FILE_ON_CRASH), -EINVAL);
    check("bad fd", kfl(-1, -1, 0, NULL, KEXEC_FILE_NO_INITRAMFS), -EBADF);
    check("cmdline not NUL terminated", kfl(kfd, -1, 4, "abcd", KEXEC_FILE_NO_INITRAMFS), -EINVAL);
    if (tfd >= 0)
        check("not an ELF", kfl(tfd, -1, 0, NULL, KEXEC_FILE_NO_INITRAMFS), -ENOEXEC);
    check_load("load again", kfl(kfd, -1, sizeof cmd, cmd, KEXEC_FILE_NO_INITRAMFS));

    pid_t p = fork();
    if (p == 0) {
        if (setuid(1000) != 0) _exit(2);
        long r = kfl(kfd, -1, 0, NULL, KEXEC_FILE_NO_INITRAMFS);
        _exit(r == -EPERM ? 0 : 1);
    }
    int st = 0;
    waitpid(p, &st, 0);
    check("non-root EPERM", WIFEXITED(st) ? WEXITSTATUS(st) : -1, 0);

    check("final unload", kfl(-1, -1, 0, NULL, KEXEC_FILE_UNLOAD), 0);
    printf(fails ? "kexectest: %d FAILED\n" : "kexectest: ALL PASS\n", fails);
    return fails != 0;
}
