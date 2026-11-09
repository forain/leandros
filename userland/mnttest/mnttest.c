/*
 * mnttest — new mount API (open_tree, move_mount, fsopen, fsconfig, fsmount,
 * fspick, mount_setattr) must return ENOSYS on both arches, so crun/runc/
 * libmount fall back to mount(2). Numbers are identical on x86_64 and the
 * generic (aarch64) table.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <stdio.h>
#include <unistd.h>
#include <sys/syscall.h>

static int fails;

static void expect_enosys(const char *name, long nr)
{
	errno = 0;
	long r = syscall(nr, 0L, 0L, 0L, 0L, 0L, 0L);
	if (r == -1 && errno == ENOSYS) {
		printf("PASS %s\n", name);
	} else {
		printf("FAIL %s: ret=%ld errno=%d\n", name, r, errno);
		fails++;
	}
}

int main(void)
{
	expect_enosys("open_tree", 428);
	expect_enosys("move_mount", 429);
	expect_enosys("fsopen", 430);
	expect_enosys("fsconfig", 431);
	expect_enosys("fsmount", 432);
	expect_enosys("fspick", 433);
	expect_enosys("mount_setattr", 442);
	printf("mnttest: %s\n", fails ? "FAILED" : "all passed");
	return fails ? 1 : 0;
}
