// kmodctl: list / load / unload / info for LeandrOS kernel modules.
//   kmodctl list              pretty /proc/modules
//   kmodctl load <path>       finit_module(fd, "", 0)
//   kmodctl unload <name>     delete_module(name, 0) (the kernel refuses: EBUSY)
//   kmodctl info <path>       dump the 128-byte module header
// Errors print strerror and the errno number; exit status 1.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static int fail(const char *what)
{
    int e = errno;
    fprintf(stderr, "kmodctl: %s: %s (errno %d)\n", what, strerror(e), e);
    return 1;
}

static int cmd_list(void)
{
    FILE *f = fopen("/proc/modules", "r");
    if (!f) return fail("/proc/modules");
    char line[256], name[64], state[16], dash[4];
    unsigned long size; int regs; unsigned long long base;
    printf("%-20s %10s %5s %-8s %s\n", "Module", "Size", "Regs", "State", "Base");
    while (fgets(line, sizeof line, f)) {
        if (sscanf(line, "%63s %lu %d %3s %15s 0x%llx", name, &size, &regs, dash, state, &base) == 6)
            printf("%-20s %10lu %5d %-8s 0x%llx\n", name, size, regs, state, base);
        else
            fputs(line, stdout);
    }
    fclose(f);
    return 0;
}

static int cmd_load(const char *path)
{
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0) return fail(path);
    long r = syscall(SYS_finit_module, fd, "", 0);
    int e = errno;
    close(fd);
    if (r < 0) { errno = e; return fail("finit_module"); }
    printf("loaded %s\n", path);
    return 0;
}

static int cmd_unload(const char *name)
{
    if (syscall(SYS_delete_module, name, 0) < 0) return fail("delete_module");
    printf("unloaded %s\n", name);
    return 0;
}

static int cmd_info(const char *path)
{
    static const unsigned char magic[8] = { 'L', 'K', 'M', 'O', 'D', 0, 0, 1 };
    FILE *f = fopen(path, "rb");
    if (!f) return fail(path);
    static unsigned char buf[65536 + 128];
    size_t n = fread(buf, 1, sizeof buf, f);
    fclose(f);
    for (size_t o = 0; o + 128 <= n; o += 16) {
        if (memcmp(buf + o, magic, 8)) continue;
        const unsigned char *h = buf + o;
        uint32_t abi, hsz, mach, flags, idlen, idn, idsz;
        uint64_t sb, ss, init, ids;
        memcpy(&abi, h + 8, 4); memcpy(&hsz, h + 12, 4); memcpy(&mach, h + 16, 4);
        memcpy(&flags, h + 20, 4); memcpy(&idlen, h + 44, 4);
        memcpy(&sb, h + 48, 8); memcpy(&ss, h + 56, 8); memcpy(&init, h + 64, 8);
        memcpy(&ids, h + 80, 8); memcpy(&idn, h + 88, 4); memcpy(&idsz, h + 92, 4);
        printf("name        %.32s\nabi         %u (header %u bytes)\nmachine     %u\nflags       0x%x\n",
               (const char *)(h + 96), abi, hsz, mach, flags);
        printf("build-id    ");
        for (uint32_t i = 0; i < idlen && i < 20; i++) printf("%02x", h[24 + i]);
        printf("\nslot        0x%llx + 0x%llx\ninit        0x%llx\nids         0x%llx (%u x %u bytes)\n",
               (unsigned long long)sb, (unsigned long long)ss, (unsigned long long)init,
               (unsigned long long)ids, idn, idsz);
        return 0;
    }
    fprintf(stderr, "kmodctl: %s: no module header found\n", path);
    return 1;
}

int main(int argc, char **argv)
{
    if (argc == 2 && !strcmp(argv[1], "list")) return cmd_list();
    if (argc == 3 && !strcmp(argv[1], "load")) return cmd_load(argv[2]);
    if (argc == 3 && (!strcmp(argv[1], "unload") || !strcmp(argv[1], "rmmod"))) return cmd_unload(argv[2]);
    if (argc == 3 && !strcmp(argv[1], "info")) return cmd_info(argv[2]);
    fprintf(stderr, "usage: kmodctl list | load <path> | unload|rmmod <name> | info <path>\n");
    return 2;
}
