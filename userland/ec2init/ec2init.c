/*
 * ec2init -- first-boot service for AWS EC2 (lane ec2init). init starts it in
 * the background at every boot; it does nothing unless this is EC2:
 *
 *   marker  kernel command line has ec2=1 (set by the Nitro platform code or
 *           the AMI) or ec2init.endpoint=HOST:PORT, or the DMI vendor
 *           (/sys/class/dmi/id/{sys_vendor,bios_vendor}) says Amazon.
 *   knobs   ec2init=0          never run
 *           ec2init.endpoint=  IMDS address, default 169.254.169.254:80
 *           ec2init.wait=N     seconds to wait for the IMDS (network/DHCP
 *                              may still be coming up), default 90
 *           ec2init.userdata=1 run user-data when it starts with "#!"
 *   test    EC2INIT_CMDLINE=FILE replaces /proc/cmdline (ec2inittest)
 *
 * IMDSv2 over plain HTTP/1.1: PUT /latest/api/token (TTL header) -> token;
 * GET meta-data/instance-id, public-keys/0/openssh-key, local-hostname, and
 * user-data, each with the token header. Once per instance id
 * (/var/lib/ec2-init/instance-id): the public key goes into
 * ~/.ssh/authorized_keys of root and leandro (appended, deduplicated), the
 * hostname (first label of local-hostname) is set and written to
 * /etc/hostname. Exit 0 done / already done / not EC2, 1 IMDS unreachable or
 * a step failed (the stamp is only written when every step worked).
 * Plain C, static musl (clang musl wrapper), POSIX sockets only.
 */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdarg.h>
#include <pwd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define BODY_MAX 65536
#define STAMP_DIR "/var/lib/ec2-init"
#define STAMP STAMP_DIR "/instance-id"

static char cmdline[4096];
static struct in_addr ep_addr;
static int ep_port = 80;
static char ep_host[64] = "169.254.169.254";
static char token[256];

static void say(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void say(const char *fmt, ...)
{
	va_list ap;
	printf("EC2INIT: ");
	va_start(ap, fmt);
	vprintf(fmt, ap);
	va_end(ap);
	printf("\n");
	fflush(stdout);
}

static long now_ms(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec * 1000L + ts.tv_nsec / 1000000L;
}

/* Value of "name=" on the command line (whole-word), or NULL. */
static const char *cmd_get(const char *name, char *out, size_t n)
{
	size_t l = strlen(name);
	for (const char *p = cmdline; *p;) {
		while (*p == ' ')
			p++;
		const char *e = p;
		while (*e && *e != ' ')
			e++;
		if ((size_t)(e - p) > l && !strncmp(p, name, l) && p[l] == '=') {
			size_t v = (size_t)(e - p) - l - 1;
			if (v >= n)
				v = n - 1;
			memcpy(out, p + l + 1, v);
			out[v] = 0;
			return out;
		}
		p = e;
	}
	return NULL;
}

static int file_has(const char *path, const char *needle)
{
	char b[256];
	FILE *f = fopen(path, "r");
	if (!f)
		return 0;
	size_t n = fread(b, 1, sizeof b - 1, f);
	fclose(f);
	b[n] = 0;
	return strstr(b, needle) != NULL;
}

static int connect_ep(int to_ms)
{
	int fd = socket(AF_INET, SOCK_STREAM, 0);
	if (fd < 0)
		return -1;
	fcntl(fd, F_SETFL, O_NONBLOCK);
	struct sockaddr_in sa = {.sin_family = AF_INET, .sin_port = htons(ep_port), .sin_addr = ep_addr};
	if (connect(fd, (struct sockaddr *)&sa, sizeof sa) < 0 && errno != EINPROGRESS) {
		close(fd);
		return -1;
	}
	struct pollfd p = {.fd = fd, .events = POLLOUT};
	int err = 0;
	socklen_t el = sizeof err;
	if (poll(&p, 1, to_ms) != 1 || getsockopt(fd, SOL_SOCKET, SO_ERROR, &err, &el) || err) {
		close(fd);
		return -1;
	}
	return fd;
}

/* One request; returns the HTTP status (0 = transport failure) and the body
 * (NUL-terminated, malloc'd into *body). */
static int http(const char *method, const char *path, const char *hdr, char **body)
{
	char req[1024];
	int n = snprintf(req, sizeof req,
			 "%s %s HTTP/1.1\r\nHost: %s\r\n%sContent-Length: 0\r\nConnection: close\r\n\r\n",
			 method, path, ep_host, hdr);
	int fd = connect_ep(3000);
	if (fd < 0)
		return 0;
	fcntl(fd, F_SETFL, 0);
	struct timeval tv = {.tv_sec = 5};
	setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
	setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof tv);
	if (write(fd, req, n) != n) {
		close(fd);
		return 0;
	}
	char *buf = malloc(BODY_MAX + 1);
	size_t len = 0;
	ssize_t r;
	while (len < BODY_MAX && (r = read(fd, buf + len, BODY_MAX - len)) > 0)
		len += r;
	close(fd);
	buf[len] = 0;
	int status = 0;
	if (len < 12 || strncmp(buf, "HTTP/1.", 7) || sscanf(buf + 9, "%d", &status) != 1) {
		free(buf);
		return 0;
	}
	char *sep = strstr(buf, "\r\n\r\n");
	char *b = sep ? sep + 4 : buf + len;
	*body = strdup(b);
	free(buf);
	return status;
}

static int get_token(void)
{
	char *b = NULL;
	int st = http("PUT", "/latest/api/token", "X-aws-ec2-metadata-token-ttl-seconds: 21600\r\n", &b);
	if (st == 200 && b && *b && strlen(b) < sizeof token) {
		strcpy(token, b);
		free(b);
		return 0;
	}
	free(b);
	return st ? -st : -1;
}

static int meta(const char *path, char **body)
{
	char hdr[320];
	snprintf(hdr, sizeof hdr, "X-aws-ec2-metadata-token: %s\r\n", token);
	return http("GET", path, hdr, body);
}

static void trim(char *s)
{
	size_t n = strlen(s);
	while (n && (s[n - 1] == '\n' || s[n - 1] == '\r' || s[n - 1] == ' '))
		s[--n] = 0;
}

static int mkdirs(const char *p)
{
	char t[256];
	snprintf(t, sizeof t, "%s", p);
	for (char *s = t + 1; *s; s++)
		if (*s == '/') {
			*s = 0;
			mkdir(t, 0755);
			*s = '/';
		}
	return mkdir(t, 0755) && errno != EEXIST ? -1 : 0;
}

/* "type base64" of an authorized_keys line (comment/options-free keys). */
static int same_key(const char *line, const char *key)
{
	char a1[16], a2[1024], b1[16], b2[1024];
	if (sscanf(line, "%15s %1023s", a1, a2) != 2 || sscanf(key, "%15s %1023s", b1, b2) != 2)
		return 0;
	return !strcmp(a1, b1) && !strcmp(a2, b2);
}

static int install_key(const char *user, const char *home_fallback, const char *key)
{
	struct passwd *pw = getpwnam(user);
	uid_t uid = pw ? pw->pw_uid : (strcmp(user, "root") ? (uid_t)-1 : 0);
	gid_t gid = pw ? pw->pw_gid : 0;
	const char *home = pw ? pw->pw_dir : home_fallback;
	if (uid == (uid_t)-1) {
		say("user %s does not exist, skipped", user);
		return 0;
	}
	char dir[256], file[300];
	snprintf(dir, sizeof dir, "%s/.ssh", home);
	snprintf(file, sizeof file, "%s/authorized_keys", dir);
	if (mkdirs(dir) < 0) {
		say("cannot create %s: %s", dir, strerror(errno));
		return -1;
	}
	chmod(dir, 0700);
	chown(dir, uid, gid);
	int present = 0;
	FILE *f = fopen(file, "r");
	if (f) {
		char l[2048];
		while (fgets(l, sizeof l, f))
			if (same_key(l, key))
				present = 1;
		fclose(f);
	}
	if (!present) {
		f = fopen(file, "a");
		if (!f) {
			say("cannot write %s: %s", file, strerror(errno));
			return -1;
		}
		fprintf(f, "%s\n", key);
		fclose(f);
	}
	chmod(file, 0600);
	chown(file, uid, gid);
	say("key %s for %s (%s)", present ? "already present" : "installed", user, file);
	return 0;
}

static void set_hostname(const char *full)
{
	char h[64];
	size_t n = 0;
	while (full[n] && full[n] != '.' && n < sizeof h - 1) {
		char c = full[n];
		if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '-'))
			return say("local-hostname %.40s rejected", full);
		h[n] = c;
		n++;
	}
	h[n] = 0;
	if (!n)
		return;
	if (sethostname(h, n))
		say("sethostname(%s): %s", h, strerror(errno));
	FILE *f = fopen("/etc/hostname", "w");
	if (f) {
		fprintf(f, "%s\n", h);
		fclose(f);
	}
	say("hostname %s", h);
}

static void run_userdata(const char *ud)
{
	char path[] = STAMP_DIR "/user-data";
	FILE *f = fopen(path, "w");
	if (!f)
		return;
	fputs(ud, f);
	fclose(f);
	chmod(path, 0700);
	if (strncmp(ud, "#!", 2)) {
		say("user-data saved (%zu bytes), not a script", strlen(ud));
		return;
	}
	pid_t p = fork();
	if (p == 0) {
		execl(path, path, (char *)NULL);
		_exit(127);
	}
	int st = 0;
	if (p > 0)
		waitpid(p, &st, 0);
	say("user-data ran, status %d", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
}

int main(void)
{
	const char *cl = getenv("EC2INIT_CMDLINE");
	FILE *f = fopen(cl ? cl : "/proc/cmdline", "r");
	if (f) {
		size_t n = fread(cmdline, 1, sizeof cmdline - 1, f);
		cmdline[n] = 0;
		fclose(f);
		trim(cmdline);
	}
	char v[128];
	if (cmd_get("ec2init", v, sizeof v) && !strcmp(v, "0"))
		return 0;
	int marker = (cmd_get("ec2", v, sizeof v) && !strcmp(v, "1")) ||
		     file_has("/sys/class/dmi/id/sys_vendor", "Amazon") ||
		     file_has("/sys/class/dmi/id/bios_vendor", "Amazon");
	if (cmd_get("ec2init.endpoint", v, sizeof v)) {
		marker = 1;
		char *c = strrchr(v, ':');
		if (c) {
			*c = 0;
			ep_port = atoi(c + 1);
		}
		snprintf(ep_host, sizeof ep_host, "%s", v);
	}
	if (!marker) {
		say("not EC2, nothing to do");
		return 0;
	}
	if (inet_pton(AF_INET, ep_host, &ep_addr) != 1) {
		say("bad endpoint %s", ep_host);
		return 1;
	}
	int wait_s = 90;
	if (cmd_get("ec2init.wait", v, sizeof v))
		wait_s = atoi(v);
	int do_ud = cmd_get("ec2init.userdata", v, sizeof v) && !strcmp(v, "1");

	/* The network may still be coming up: retry the token request. */
	long deadline = now_ms() + wait_s * 1000L;
	int rc;
	for (;;) {
		rc = get_token();
		if (rc == 0 || now_ms() >= deadline)
			break;
		usleep(1000 * 1000);
	}
	if (rc) {
		say("IMDS %s:%d unreachable or refused the token (%d), giving up", ep_host, ep_port, rc);
		return 1;
	}
	char *id = NULL;
	if (meta("/latest/meta-data/instance-id", &id) != 200 || !id || !*id) {
		say("no instance-id from the IMDS");
		return 1;
	}
	trim(id);
	char old[128] = "";
	f = fopen(STAMP, "r");
	if (f) {
		if (fgets(old, sizeof old, f))
			trim(old);
		fclose(f);
	}
	if (!strcmp(old, id)) {
		say("instance %s already provisioned", id);
		return 0;
	}
	say("first boot of instance %s (was '%s')", id, old);
	int bad = 0;
	char *key = NULL;
	int st = meta("/latest/meta-data/public-keys/0/openssh-key", &key);
	if (st == 200 && key && *key) {
		trim(key);
		if (install_key("root", "/root", key) || install_key("leandro", "/home/leandro", key))
			bad = 1;
	} else if (st == 404) {
		say("instance has no key pair");
	} else {
		say("public key request failed (%d)", st);
		bad = 1;
	}
	char *hn = NULL;
	if (meta("/latest/meta-data/local-hostname", &hn) == 200 && hn) {
		trim(hn);
		set_hostname(hn);
	}
	mkdirs(STAMP_DIR);
	if (do_ud) {
		char *ud = NULL;
		if (meta("/latest/user-data", &ud) == 200 && ud && *ud)
			run_userdata(ud);
	}
	if (bad)
		return 1;
	f = fopen(STAMP, "w");
	if (!f) {
		say("cannot write %s: %s", STAMP, strerror(errno));
		return 1;
	}
	fprintf(f, "%s\n", id);
	fclose(f);
	say("done");
	return 0;
}
