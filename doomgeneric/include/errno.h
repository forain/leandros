#ifndef _ERRNO_H
#define _ERRNO_H

extern int *__errno_location(void);
#define errno (*__errno_location())

#define EPERM   1
#define ENOENT  2
#define ESRCH   3
#define EINTR   4
#define EIO     5
#define EBADF   9
#define ECHILD  10
#define EAGAIN  11
#define ENOMEM  12
#define EACCES  13
#define EFAULT  14
#define EBUSY   16
#define EEXIST  17
#define EINVAL  22
#define ENOSPC  28
#define EPIPE   32
#define ENOSYS  38
#define ENOTEMPTY 39

#endif
