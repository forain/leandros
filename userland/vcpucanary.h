/* vcpucanary.h -- tell host scheduling delay from a guest wake-up bug.
 *
 * Two canary threads run for the life of a latency-limited test:
 *   spin:  busy-loops on CLOCK_MONOTONIC and records every gap > SPIN_MIN_US
 *          between consecutive reads. A spinning thread is never waiting for a
 *          wake-up, so a gap means its vCPU did not run (host steal, or the
 *          guest preempting it, which is rare on an idle -smp 4 guest).
 *   sleep: nanosleep(1 ms) loop recording the overshoot. Overshoot WITHOUT a
 *          concurrent spin gap points at the guest wake-up path.
 * canary_report(t0, t1, buf) summarises both over a window [t0, t1] (monotonic
 * microseconds, as now_us()). Header-only; include once per program.
 */
#ifndef VCPUCANARY_H
#define VCPUCANARY_H
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <time.h>

#define CANARY_N 8192
#define SPIN_MIN_US 1000
#define SLEEP_MIN_US 2000

struct canary_ev { long long end, len; };
static struct canary_ev canary_spin_ev[CANARY_N], canary_slp_ev[CANARY_N];
static volatile unsigned canary_spin_n, canary_slp_n;
static volatile int canary_stop_flag;
static pthread_t canary_th[2];
static int canary_running;

static long long canary_now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000 + ts.tv_nsec / 1000;
}
static void *canary_spin(void *a) {
    (void)a;
#ifdef SCHED_IDLE
    { struct sched_param sp = { 0 }; pthread_setschedparam(pthread_self(), SCHED_IDLE, &sp); } /* best effort */
#endif
    long long prev = canary_now();
    while (!canary_stop_flag) {
        long long t = canary_now();
        if (t - prev > SPIN_MIN_US) {
            unsigned i = canary_spin_n;
            canary_spin_ev[i % CANARY_N] = (struct canary_ev){ t, t - prev };
            canary_spin_n = i + 1;
        }
        prev = t;
    }
    return NULL;
}
static void *canary_sleep(void *a) {
    (void)a;
    while (!canary_stop_flag) {
        long long t0 = canary_now();
        struct timespec ts = { 0, 1000000 };
        nanosleep(&ts, NULL);
        long long t1 = canary_now(), over = t1 - t0 - 1000;
        if (over > SLEEP_MIN_US) {
            unsigned i = canary_slp_n;
            canary_slp_ev[i % CANARY_N] = (struct canary_ev){ t1, over };
            canary_slp_n = i + 1;
        }
    }
    return NULL;
}
static void canary_start(void) {
    if (canary_running) return;
    canary_stop_flag = 0;
    pthread_create(&canary_th[0], NULL, canary_spin, NULL);
    pthread_create(&canary_th[1], NULL, canary_sleep, NULL);
    canary_running = 1;
}
static void canary_stop(void) {
    if (!canary_running) return;
    canary_stop_flag = 1;
    pthread_join(canary_th[0], NULL);
    pthread_join(canary_th[1], NULL);
    canary_running = 0;
}
static long long canary_max(const struct canary_ev *ev, volatile unsigned *np, long long t0, long long t1) {
    unsigned n = *np, lo = n > CANARY_N ? n - CANARY_N : 0;
    long long m = 0;
    for (unsigned i = lo; i < n; i++) {
        const struct canary_ev *e = &ev[i % CANARY_N];
        if (e->end >= t0 && e->end - e->len <= t1 && e->len > m) m = e->len;
    }
    return m;
}
/* "vcpu-delay: spin-gap X us, sleep-overshoot Y us [HOST-DELAYED|KERNEL?]" for window [t0,t1]. */
static const char *canary_report(long long t0, long long t1, char *buf, size_t sz) {
    long long sg = canary_max(canary_spin_ev, &canary_spin_n, t0, t1);
    long long so = canary_max(canary_slp_ev, &canary_slp_n, t0, t1);
    /* The sleeper's overshoot includes any vCPU stall the spinner saw; only the
     * residue (overshoot - gap) is a guest wake-up delay. */
    long long res = so > sg ? so - sg : 0;
    char vb[96];
    const char *v;
    if (sg >= SPIN_MIN_US && res < SLEEP_MIN_US) v = "HOST-DELAYED";
    else if (sg >= SPIN_MIN_US) { snprintf(vb, sizeof vb, "MIXED (host %lld ms + wake %lld ms)", sg / 1000, res / 1000); v = vb; }
    else v = so >= SLEEP_MIN_US ? "NO-SPIN-GAP(kernel-wake?)" : "no-vcpu-delay";
    snprintf(buf, sz, "[canary spin-gap %lld us, sleep-overshoot %lld us, residue %lld us: %s]", sg, so, res, v);
    return buf;
}
#endif
