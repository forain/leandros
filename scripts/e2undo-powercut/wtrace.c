/* LD_PRELOAD write tracer: records, in order, every write/fsync/truncate that a
 * process makes to the files named in $WT_FILES (comma list of basenames) into
 * $WT_LOG, so a power cut can be replayed at any point. Record = op(1) file(1)
 * pad(6) off(8) len(8) [data]. op 1 write, 2 fsync/fdatasync, 3 ftruncate. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

#define MAXFD 4096
static signed char fdmap[MAXFD];
static int logfd = -1;
static char *names[8];
static int nnames;

__attribute__((constructor)) static void init(void)
{
	memset(fdmap, -1, sizeof fdmap);
	const char *l = getenv("WT_LOG"), *f = getenv("WT_FILES");
	if (!l || !f) return;
	logfd = syscall(SYS_open, l, O_WRONLY | O_CREAT | O_TRUNC | O_APPEND, 0644);
	if (logfd >= 512) { int n = fcntl(logfd, F_DUPFD, 3); (void)n; }
	char *s = strdup(f), *p;
	for (p = strtok(s, ","); p && nnames < 8; p = strtok(NULL, ",")) names[nnames++] = p;
}
static void rec(int op, int file, uint64_t off, uint64_t len, const void *data, uint64_t dlen)
{
	unsigned char h[24] = {0};
	h[0] = op; h[1] = file;
	memcpy(h + 8, &off, 8); memcpy(h + 16, &len, 8);
	syscall(SYS_write, logfd, h, 24);
	if (dlen) syscall(SYS_write, logfd, data, dlen);
}
static void note_open(int fd, const char *path)
{
	if (fd < 0 || fd >= MAXFD || logfd < 0) return;
	const char *b = strrchr(path, '/'); b = b ? b + 1 : path;
	fdmap[fd] = -1;
	for (int i = 0; i < nnames; i++) if (!strcmp(b, names[i])) fdmap[fd] = i;
}
static int tr(int fd) { return (fd >= 0 && fd < MAXFD && logfd >= 0) ? fdmap[fd] : -1; }

int open(const char *p, int fl, ...) { va_list a; va_start(a, fl); mode_t m = va_arg(a, mode_t); va_end(a);
	int fd = syscall(SYS_openat, AT_FDCWD, p, fl, m); note_open(fd, p); return fd; }
int open64(const char *p, int fl, ...) { va_list a; va_start(a, fl); mode_t m = va_arg(a, mode_t); va_end(a);
	int fd = syscall(SYS_openat, AT_FDCWD, p, fl, m); note_open(fd, p); return fd; }
int openat(int d, const char *p, int fl, ...) { va_list a; va_start(a, fl); mode_t m = va_arg(a, mode_t); va_end(a);
	int fd = syscall(SYS_openat, d, p, fl, m); note_open(fd, p); return fd; }
int close(int fd) { if (fd >= 0 && fd < MAXFD) fdmap[fd] = -1; return syscall(SYS_close, fd); }
ssize_t pwrite(int fd, const void *b, size_t n, off_t o)
{ ssize_t r = syscall(SYS_pwrite64, fd, b, n, o); int t = tr(fd); if (t >= 0 && r > 0) rec(1, t, o, r, b, r); return r; }
ssize_t pwrite64(int fd, const void *b, size_t n, off_t o) { return pwrite(fd, b, n, o); }
ssize_t write(int fd, const void *b, size_t n)
{ int t = tr(fd); off_t o = t >= 0 ? lseek(fd, 0, SEEK_CUR) : 0;
  ssize_t r = syscall(SYS_write, fd, b, n); if (t >= 0 && r > 0) rec(1, t, o, r, b, r); return r; }
int fsync(int fd) { int t = tr(fd); int r = syscall(SYS_fsync, fd); if (t >= 0 && !r) rec(2, t, 0, 0, 0, 0); return r; }
int fdatasync(int fd) { int t = tr(fd); int r = syscall(SYS_fdatasync, fd); if (t >= 0 && !r) rec(2, t, 0, 0, 0, 0); return r; }
int ftruncate(int fd, off_t l) { int t = tr(fd); int r = syscall(SYS_ftruncate, fd, l); if (t >= 0 && !r) rec(3, t, 0, l, 0, 0); return r; }
int ftruncate64(int fd, off_t l) { return ftruncate(fd, l); }
