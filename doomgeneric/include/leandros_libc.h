#ifndef _LEANDROS_LIBC_H
#define _LEANDROS_LIBC_H

#include <stddef.h>
#include <stdarg.h>
#include <stdint.h>

// stdio.h
typedef void FILE;
extern FILE *stderr;
extern FILE *stdout;
int printf(const char *format, ...);
int fprintf(FILE *stream, const char *format, ...);
int puts(const char *s);
int putchar(int c);
int snprintf(char *str, size_t size, const char *format, ...);
int sscanf(const char *str, const char *format, ...);
typedef __builtin_va_list va_list;
int vsnprintf(char *str, size_t size, const char *format, va_list ap);
int vfprintf(FILE *stream, const char *format, va_list ap);
FILE *fopen(const char *path, const char *mode);
int fclose(FILE *stream);
size_t fread(void *ptr, size_t size, size_t nmemb, FILE *stream);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
int fseek(FILE *stream, long offset, int whence);
long ftell(FILE *stream);
void fflush(FILE *stream);
int remove(const char *pathname);
int rename(const char *oldpath, const char *newpath);
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2

// string.h
void *memset(void *s, int c, size_t n);
void *memcpy(void *dest, const void *src, size_t n);
void *memmove(void *dest, const void *src, size_t n);
char *strncpy(char *dest, const char *src, size_t n);
char *strcpy(char *dest, const char *src);
size_t strlen(const char *s);
int strcmp(const char *s1, const char *s2);
int strncmp(const char *s1, const char *s2, size_t n);
int strcasecmp(const char *s1, const char *s2);
int strncasecmp(const char *s1, const char *s2, size_t n);
char *strdup(const char *s);
char *strchr(const char *s, int c);
char *strrchr(const char *s, int c);
char *strstr(const char *haystack, const char *needle);

// stdlib.h
void *malloc(size_t size);
void *calloc(size_t nmemb, size_t size);
void free(void *ptr);
void exit(int status);
int system(const char* command);
int abs(int j);
int atoi(const char *nptr);
double atof(const char *nptr);
long strtol(const char *nptr, char **endptr, int base);

// unistd.h
int open(const char* path, int flags, int mode);
int close(int fd);
long write(int fd, const void* buf, size_t count);
long read(int fd, void* buf, size_t count);
int lseek(int fd, long offset, int whence);
int usleep(unsigned int usec);
int mkdir(const char *path, int mode);
int access(const char *path, int amode);

// fcntl.h

#define O_RDONLY 0
#define O_WRONLY 1
#define O_RDWR   2

// time.h
struct timespec {
    long tv_sec;
    long tv_nsec;
};
int clock_gettime(int clk_id, struct timespec *tp);

// ctype.h
int isspace(int c);
int isdigit(int c);
int toupper(int c);
int tolower(int c);

// sys/ioctl.h
int ioctl(int fd, unsigned long request, ...);

// math.h
double fabs(double x);

#endif
