/* floattest: C callers of leandros-libc must get the SysV/AAPCS64 float ABI.
 *
 * Built like DOOM (clang, -ffreestanding -nostdlib, linked against
 * libleandros_libc.a). On x86_64 the libc was once built soft-float, so every
 * exported double function and compiler-rt float builtin read its argument
 * from an integer register (lane kvmaudio / floatabi, 2026-10-24).
 * Prints "<case>: PASS|FAIL" lines and exits non-zero on any failure.
 */
typedef unsigned long size_t;
int printf(const char *fmt, ...);
int snprintf(char *buf, size_t n, const char *fmt, ...);
double pow(double, double);
double exp(double);
double log(double);
double log10(double);
double tan(double);
double sqrt(double);

static int failures;

static int str_eq(const char *a, const char *b) {
    while (*a && *a == *b) { a++; b++; }
    return *a == *b;
}

/* Compare with a relative tolerance without calling any libm function. */
static void check(const char *name, double got, double want) {
    double d = got - want, m = want < 0 ? -want : want;
    if (d < 0) d = -d;
    int ok = d <= 1e-12 * (m > 1 ? m : 1);
    printf("%s: %s\n", name, ok ? "PASS" : "FAIL");
    if (!ok) failures++;
}

static void check_fmt(const char *name, const char *got, const char *want) {
    int ok = str_eq(got, want);
    if (ok) printf("%s: PASS\n", name);
    else { printf("%s: FAIL got '%s' want '%s'\n", name, got, want); failures++; }
}

/* Opaque inputs so clang cannot constant-fold the libm calls. */
volatile double v_two = 2.0, v_ten = 10.0, v_one = 1.0, v_quarter = 0.25,
                v_neg = -1.5, v_e = 2.718281828459045, v_big = 12345.678;
volatile int v_seven = 7;
volatile long long v_i128_hi = 3;

int main(void) {
    check("pow", pow(v_two, v_ten), 1024.0);
    check("exp", exp(v_one), 2.718281828459045);
    check("log", log(v_e), 1.0);
    check("log10", log10(v_ten * v_ten), 2.0);
    check("tan", tan(v_quarter), 0.25534192122103627);
    check("sqrt", sqrt(v_two * v_two), 2.0);
    /* compiler-rt float builtins the libc archive provides */
    check("powidf2", __builtin_powi(v_two, v_seven), 128.0);
    __int128 big = (__int128)v_i128_hi << 64;
    check("floattidf", (double)big, 55340232221128654848.0);
    check("fixdfti", (double)(__int128)(v_big * 1e20), v_big * 1e20);

    char buf[128];
    snprintf(buf, sizeof buf, "%f", v_neg);
    check_fmt("printf_f", buf, "-1.500000");
    snprintf(buf, sizeof buf, "%.2f|%d|%.3f", v_big, 42, v_quarter);
    check_fmt("printf_mixed", buf, "12345.68|42|0.250");
    snprintf(buf, sizeof buf, "%e", v_big);
    check_fmt("printf_e", buf, "1.234568e+04");
    snprintf(buf, sizeof buf, "%g %g %g", v_big, v_quarter, v_big * 1e10);
    check_fmt("printf_g", buf, "12345.7 0.25 1.23457e+14");
    snprintf(buf, sizeof buf, "%8.3f|%-8.1f|", v_two, v_neg);
    check_fmt("printf_width", buf, "   2.000|-1.5    |");

    /* Variadic arguments that spill to the stack (SysV x86_64: >5 ints after
     * buf/n/fmt, >8 doubles; AAPCS64: >7/8). Each class keeps its own order. */
    snprintf(buf, sizeof buf, "%d %d %d %d %d %d %d %d", 1, 2, 3, 4, 5, 6, 7, 8);
    check_fmt("va_8ints", buf, "1 2 3 4 5 6 7 8");
    snprintf(buf, sizeof buf, "%u %u %u %u %u", 1u, 2u, 3u, 4u, 5u);
    check_fmt("va_5uints", buf, "1 2 3 4 5");
    snprintf(buf, sizeof buf, "%.1f %.1f %.1f %.1f %.1f %.1f %.1f %.1f %.1f %.1f",
             1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0);
    check_fmt("va_10doubles", buf, "1.0 2.0 3.0 4.0 5.0 6.0 7.0 8.0 9.0 10.0");
    snprintf(buf, sizeof buf,
             "%d %f %s %ld %g %c %u %e %x %f %d %f %d %f %g %g %f %f %d",
             -1, 1.5, "str", 123456789012L, 0.25, 'Z', 7u, 12345.0, 0xbeefu, 2.5,
             -2, 3.5, -3, 4.5, 5.5, 6.5, 7.5, 8.5, -4);
    check_fmt("va_mixed", buf,
              "-1 1.500000 str 123456789012 0.25 Z 7 1.234500e+04 beef 2.500000 "
              "-2 3.500000 -3 4.500000 5.5 6.5 7.500000 8.500000 -4");
    {
        long long ll = 0x123456789abcdefLL;
        int tag = 5;
        snprintf(buf, sizeof buf, "%lld %lld %lld %x %d %lld %p %s %llx", ll, ll + 1, ll + 2,
                 0xaau, tag, -ll, (void *)0x1000, "end", (unsigned long long)ll);
        check_fmt("va_longlong_ptr", buf,
                  "81985529216486895 81985529216486896 81985529216486897 aa 5 "
                  "-81985529216486895 0x1000 end 123456789abcdef");
    }

    printf("floattest: %s (%d failures)\n", failures ? "FAIL" : "PASS", failures);
    return failures ? 1 : 0;
}
