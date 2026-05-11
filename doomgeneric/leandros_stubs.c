#include "leandros_libc.h"

void* stderr = (void*)2;
void* stdout = (void*)1;

const unsigned short ** __ctype_toupper_loc(void) {
    static const unsigned short upper[256] = {
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
        16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
        ' ', '!', '"', '#', '$', '%', '&', '\'', '(', ')', '*', '+', ',', '-', '.', '/',
        '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', ':', ';', '<', '=', '>', '?',
        '@', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O',
        'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '[', '\\', ']', '^', '_',
        '`', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O',
        'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '{', '|', '}', '~', 127
    };
    static const unsigned short *p_upper = upper;
    return &p_upper;
}

void fflush(FILE* stream) {}

int putc(int c, FILE* stream) {
    char b = (char)c;
    write(stream == stderr ? 2 : 1, &b, 1);
    return c;
}

int vfprintf(FILE* stream, const char* fmt, va_list ap) {
    // Print format string and first string argument so I_Error is visible.
    write(2, fmt, strlen(fmt));
    // Try to print the first va_arg as a string (best-effort for error messages).
    const char* arg = va_arg(ap, const char*);
    if (arg) { write(2, ": ", 2); write(2, arg, strlen(arg)); }
    write(2, "\n", 1);
    return 0;
}

int vsnprintf(char *str, size_t size, const char *format, va_list ap) {
    if (size == 0) return 0;
    size_t out = 0;
    const char *p = format;
    while (*p && out < size - 1) {
        if (*p == '%' && *(p + 1)) {
            p++;
            if (*p == 'd' || *p == 'i') {
                int val = va_arg(ap, int);
                char tmp[20];
                int n = 0;
                if (val < 0) { tmp[n++] = '-'; val = -val; }
                if (val == 0) { tmp[n++] = '0'; }
                else { int v = val; while (v > 0) { tmp[n++] = '0' + v % 10; v /= 10; }
                       for (int i = 0, j = n-1; i < j; i++, j--) { char t = tmp[i]; tmp[i] = tmp[j]; tmp[j] = t; } }
                for (int i = 0; i < n && out < size - 1; i++) str[out++] = tmp[i];
            } else if (*p == 's') {
                const char *s = va_arg(ap, const char *);
                if (s) while (*s && out < size - 1) str[out++] = *s++;
            } else if (*p == 'c') {
                char c = (char)va_arg(ap, int);
                if (out < size - 1) str[out++] = c;
            } else if (*p == '%') {
                if (out < size - 1) str[out++] = '%';
            } else {
                // Skip unknown format specs; consume one argument
                va_arg(ap, int);
            }
        } else {
            str[out++] = *p;
        }
        p++;
    }
    str[out] = 0;
    return (int)out;
}

long __isoc23_strtol(const char *nptr, char **endptr, int base) {
    return strtol(nptr, endptr, base);
}

int system(const char* command) {
    return -1;
}

int access(const char *path, int amode) {
    return -1;
}

int __isoc23_sscanf(const char *str, const char *format, ...) {
    return 0;
}

int sscanf(const char *str, const char *format, ...) {
    return 0;
}

double atof(const char *nptr) {
    return 0.0;
}

double fabs(double x) {
    return x < 0 ? -x : x;
}

double strtod(const char *nptr, char **endptr) {
    if (endptr) *endptr = (char*)nptr;
    return 0.0;
}

int isspace(int c) {
    return c == ' ' || c == '\t' || c == '\n' || c == '\v' || c == '\f' || c == '\r';
}

int abs(int j) {
    return j < 0 ? -j : j;
}

int toupper(int c) {
    if (c >= 'a' && c <= 'z') return c - ('a' - 'A');
    return c;
}

int strcasecmp(const char *s1, const char *s2) {
    while (*s1 && *s2) {
        char c1 = *s1;
        char c2 = *s2;
        if (c1 >= 'A' && c1 <= 'Z') c1 += 'a' - 'A';
        if (c2 >= 'A' && c2 <= 'Z') c2 += 'a' - 'A';
        if (c1 != c2) return c1 - c2;
        s1++; s2++;
    }
    return (unsigned char)*s1 - (unsigned char)*s2;
}

int strncasecmp(const char *s1, const char *s2, size_t n) {
    while (n > 0 && *s1 && *s2) {
        char c1 = *s1;
        char c2 = *s2;
        if (c1 >= 'A' && c1 <= 'Z') c1 += 'a' - 'A';
        if (c2 >= 'A' && c2 <= 'Z') c2 += 'a' - 'A';
        if (c1 != c2) return c1 - c2;
        s1++; s2++; n--;
    }
    if (n == 0) return 0;
    return (unsigned char)*s1 - (unsigned char)*s2;
}

// Dummy symbols for things that seem to be missing
int drone = 0;
int net_client_connected = 0;
void WI_Drawer() {}
void WI_End() {}
void WI_Start() {}
void WI_Ticker() {}
void StatDump() {}
void StatCopy() {}
void W_Checksum() {}

// FILE stubs for things that still use them but shouldn't
FILE* fopen(const char* path, const char* mode) { return NULL; }
int fclose(FILE* stream) { return 0; }
size_t fread(void* ptr, size_t size, size_t nmemb, FILE* stream) { return 0; }
size_t fwrite(const void* ptr, size_t size, size_t nmemb, FILE* stream) { return 0; }
int fseek(FILE* stream, long offset, int whence) { return 0; }
long ftell(FILE* stream) { return 0; }
int remove(const char* pathname) { return -1; }
int rename(const char* oldpath, const char* newpath) { return -1; }
