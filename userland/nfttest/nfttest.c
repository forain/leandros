/*
 * nfttest: NETLINK_NETFILTER / nf_tables control plane (podnat L1).
 * Replays the netavark ground-truth rulesets (nft-captured on Linux) with the
 * real `nft` binary and diffs `nft -j list ruleset` against the capture, modulo
 * handles. Also checks idempotent re-add, teardown by handle, and insert /
 * position ordering. Needs Alpine's nft (/usr/sbin/nft).
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include "nfttest-data.h"

static int npass, nfail;
#define CHECK(name, cond) do { if (cond) { npass++; printf("PASS %s\n", name); } else { nfail++; printf("FAIL %s\n", name); } fflush(stdout); } while (0)

#define BUFSZ (64 * 1024)
static char got[BUFSZ], want[BUFSZ];

static int run(const char *cmd, char *out, size_t cap)
{
    FILE *p = popen(cmd, "r");
    size_t n = 0;
    if (!p) return -1;
    if (out) {
        size_t r;
        while (n + 1 < cap && (r = fread(out + n, 1, cap - 1 - n, p)) > 0) n += r;
        out[n] = 0;
    }
    char junk[256];
    while (fread(junk, 1, sizeof junk, p) > 0) {}
    int st = pclose(p);
    return WIFEXITED(st) ? WEXITSTATUS(st) : -1;
}

static void wfile(const char *path, const char *s)
{
    FILE *f = fopen(path, "w");
    if (f) { fputs(s, f); fclose(f); }
}

/* Drop `, "handle": N` and the leading {"metainfo": ...}, entry; in place. */
static void norm(char *s)
{
    char *m = strstr(s, "{\"metainfo\"");
    if (m) {
        char *e = strstr(m, "}}, ");
        if (e) memmove(m, e + 4, strlen(e + 4) + 1);
    }
    char *r = s, *w = s;
    while (*r) {
        if (!strncmp(r, ", \"handle\": ", 12)) {
            r += 12;
            while (*r >= '0' && *r <= '9') r++;
            continue;
        }
        *w++ = *r++;
    }
    *w = 0;
    while (w > s && (w[-1] == '\n' || w[-1] == ' ')) *--w = 0;
}

static void showdiff(const char *a, const char *b)
{
    size_t i = 0;
    while (a[i] && a[i] == b[i]) i++;
    size_t s = i > 70 ? i - 70 : 0;
    printf("  first diff at %zu\n  got : %.160s\n  want: %.160s\n", i, a + s, b + s);
}

static int apply_json(const char *json)
{
    wfile("/tmp/nft-in.json", json);
    return run("nft -j -f /tmp/nft-in.json 2>&1", got, sizeof got);
}

static int same(const char *expect_json, const char *label)
{
    char name[96];
    int ok;
    run("nft -j list ruleset 2>&1", got, sizeof got);
    strncpy(want, expect_json, sizeof want - 1);
    norm(got); norm(want);
    ok = !strcmp(got, want);
    snprintf(name, sizeof name, "%s: nft -j list ruleset == capture", label);
    CHECK(name, ok);
    if (!ok) showdiff(got, want);
    return ok;
}

static void scenario(const char *label, const char *json)
{
    char name[96];
    run("nft flush ruleset 2>&1", got, sizeof got);
    int rc = apply_json(json);
    snprintf(name, sizeof name, "%s: nft -j -f succeeds", label);
    CHECK(name, rc == 0);
    if (rc) { printf("  nft: %.300s\n", got); return; }
    same(json, label);
}

int main(void)
{
    char out[4096];
    int rc = run("nft --version 2>&1", out, sizeof out);
    printf("nfttest: %s", out);
    CHECK("nft binary present", rc == 0);
    if (rc) { printf("nfttest: %d passed, %d failed\n", npass, nfail); return 1; }

    CHECK("empty ruleset lists", run("nft flush ruleset 2>&1", out, sizeof out) == 0 && run("nft -j list ruleset 2>&1", out, sizeof out) == 0);

    scenario("a_bridge_noports", cap_a);
    scenario("b_port8080", cap_b);
    scenario("c_range", cap_c);
    scenario("d_udp_loopback", cap_d);
    scenario("e_internal", cap_e);
    scenario("g_two_nets_a", cap_ga);
    scenario("g_two_nets_b", cap_gb);

    /* idempotence: netavark re-adds the skeleton on every run; nothing may duplicate. */
    run("nft flush ruleset 2>&1", out, sizeof out);
    apply_json(cap_b);
    rc = run("nft add table inet netavark 2>&1 && "
             "nft add chain inet netavark FORWARD '{ type filter hook forward priority 0; policy accept; }' 2>&1 && "
             "nft add chain inet netavark NETAVARK-HOSTPORT-DNAT 2>&1 && "
             "nft add chain inet netavark NETAVARK-ISOLATION-1 2>&1", out, sizeof out);
    CHECK("idempotent: re-add table/chains succeeds", rc == 0);
    if (rc) printf("  nft: %.300s\n", out);
    same(cap_b, "idempotent re-add");

    /* netavark-style duplicate check: list the table, a rule's JSON must already be there. */
    run("nft -j list table inet netavark 2>&1", got, sizeof got);
    CHECK("list table inet netavark has anonymous-set-free dnat rule", strstr(got, "\"dnat\"") != NULL);
    CHECK("list table: no __set names leak", strstr(got, "__set") == NULL);

    /* teardown by handle (netavark: list, then delete rules, then chains). */
    run("nft flush ruleset 2>&1", out, sizeof out);
    apply_json(cap_b);
    rc = run("nft -a list table inet netavark 2>&1 | awk '"
             "/^\\tchain /{c=$2; next} /# handle/ && c!=\"\" && !/type / && "
             "(c ~ /^nv_/ || /nv_/ || /10\\.89\\.0\\.0\\/24/ || /podman1/ || /dport 8080/) "
             "{print \"delete rule inet netavark \" c \" handle \" $NF}' > /tmp/nft-td.nft; "
             "nft -f /tmp/nft-td.nft 2>&1", out, sizeof out);
    CHECK("teardown: delete rules by handle", rc == 0);
    if (rc) printf("  nft: %.300s\n", out);
    rc = run("for c in $(nft list table inet netavark | awk '/^\\tchain nv_/{print $2}'); do nft delete chain inet netavark $c || exit 1; done", out, sizeof out);
    CHECK("teardown: delete per-network chains", rc == 0);
    if (rc) printf("  nft: %.300s\n", out);
    same(cap_td, "b_teardown");

    /* named set elements round trip */
    run("nft flush ruleset 2>&1", out, sizeof out);
    run("nft add table inet t 2>&1", out, sizeof out);
    run("nft --debug=netlink add set inet t s '{ type inet_proto; elements = { tcp, udp }; }' 2>&1 | head -c 1500", got, sizeof got);
    printf("DBG %s\n", got);
    run("nft list set inet t s 2>&1", got, sizeof got);
    CHECK("named set keeps both elements", strstr(got, "tcp") && strstr(got, "udp"));
    if (!(strstr(got, "tcp") && strstr(got, "udp"))) printf("%.300s\n", got);

    /* rule order: append, insert (prepend) and position. */
    run("nft flush ruleset 2>&1", out, sizeof out);
    rc = run("nft add table inet t && nft add chain inet t c && "
             "nft add rule inet t c tcp dport 1 accept && "
             "nft insert rule inet t c tcp dport 2 accept && "
             "nft add rule inet t c tcp dport 3 accept && "
             "nft insert rule inet t c tcp dport 4 accept 2>&1", out, sizeof out);
    CHECK("order: add/insert accepted", rc == 0);
    run("nft -a list chain inet t c 2>&1", got, sizeof got);
    char *p4 = strstr(got, "dport 4"), *p2 = strstr(got, "dport 2"), *p1 = strstr(got, "dport 1"), *p3 = strstr(got, "dport 3");
    CHECK("order: 4,2,1,3", p4 && p2 && p1 && p3 && p4 < p2 && p2 < p1 && p1 < p3);
    if (!(p4 && p2 && p1 && p3 && p4 < p2 && p2 < p1 && p1 < p3)) printf("%.400s\n", got);
    /* position: handle of "dport 1" */
    char hbuf[32] = "0";
    if (p1) { char *h = strstr(p1, "handle "); if (h) snprintf(hbuf, sizeof hbuf, "%d", atoi(h + 7)); }
    char cmd[256];
    snprintf(cmd, sizeof cmd, "nft add rule inet t c position %s tcp dport 5 accept 2>&1 && nft insert rule inet t c position %s tcp dport 6 accept 2>&1", hbuf, hbuf);
    rc = run(cmd, out, sizeof out);
    CHECK("order: position accepted", rc == 0);
    run("nft list chain inet t c 2>&1", got, sizeof got);
    {
        char *a = strstr(got, "dport 6"), *b = strstr(got, "dport 1"), *c = strstr(got, "dport 5");
        CHECK("order: insert position before, add position after (6,1,5)", a && b && c && a < b && b < c);
        if (!(a && b && c && a < b && b < c)) printf("%.400s\n", got);
    }

    /* delete a chain still referenced is EBUSY, a missing one ENOENT, flush works. */
    run("nft flush ruleset 2>&1", out, sizeof out);
    run("nft add table inet t && nft add chain inet t a && nft add chain inet t b && nft add rule inet t a jump b", out, sizeof out);
    CHECK("delete referenced chain fails", run("nft delete chain inet t b 2>&1", out, sizeof out) != 0);
    CHECK("delete missing chain fails", run("nft delete chain inet t nope 2>&1", out, sizeof out) != 0);
    CHECK("flush chain then delete", run("nft flush chain inet t a && nft delete chain inet t b && nft delete chain inet t a", out, sizeof out) == 0);
    CHECK("delete table", run("nft delete table inet t && nft -j list ruleset | grep -vq '\"table\"'", out, sizeof out) == 0);

    /* an expression outside the supported subset is refused loudly. */
    run("nft flush ruleset 2>&1", out, sizeof out);
    run("nft add table inet t && nft add chain inet t c", out, sizeof out);
    CHECK("unsupported expression refused (limit)", run("nft add rule inet t c limit rate 1/second accept 2>&1", out, sizeof out) != 0);
    run("nft flush ruleset 2>&1", out, sizeof out);

    printf("nfttest: %d passed, %d failed\n", npass, nfail);
    return nfail ? 1 : 0;
}
