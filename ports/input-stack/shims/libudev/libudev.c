/*
 * libudev.c — LeandrOS libudev ABI shim (soname libudev.so.1).
 *
 * Implements the subset of libudev that libinput, smithay's `udev` crate, and
 * Mesa actually import, plus safe stubs for the rest of the public ABI so that
 * *any* consumer links. There is no udevd and no netlink uevent source on
 * LeandrOS; instead this shim serves a small device model:
 *
 *     /dev/dri/card0        subsystem=drm    (GPU scanout node, static table)
 *     /dev/dri/renderD128   subsystem=drm    (render node, static table)
 *     /dev/input/eventN     subsystem=input  every evdev node the kernel has
 *
 * Input nodes are dynamic: enumeration reads the coldplug records of the
 * kernel's /dev/input/hotplug node and derives each node's ID_INPUT_* from
 * its EVIOCGBIT capabilities (see "Dynamic input model"); an input monitor
 * polls that node and delivers "add"/"remove" devices, so libinput sees USB
 * HID devices come and go. event0 (aggregate keyboard) and event1 (virtio-
 * tablet: absolute ABS 0..32767 + BTN_LEFT, no INPUT_PROP_DIRECT, hence
 * ID_INPUT_MOUSE) are always present. A drm-only monitor never fires.
 *
 * SYSPATH CONTRACT (matches the plan's future synthetic sysfs):
 *   /sys/class/drm/<name>    for drm nodes   (e.g. /sys/class/drm/card0)
 *   /sys/class/input/<name>  for input nodes (e.g. /sys/class/input/event0)
 *   Parent input devices live at /sys/class/input/input<N>; the drm nodes'
 *   parent is a synthetic platform GPU at /sys/devices/platform/gpu.
 * When the kernel grows a real read-only synthetic sysfs, replace the static
 * table below with a directory scan of /sys/class/{drm,input}; every getter
 * here already keys off these exact paths.
 *
 * DERIVATION OF THE IMPORT SET (documented in NOTES.md):
 *   - libinput 1.27.1  (src tree)  grep 'udev_[a-z_]+'  (real libudev calls only)
 *   - udev crate 0.9.3 (smithay)  grep of its FFI usage
 *   - libudev-sys 0.1.4           full extern "C" block (82 symbols) — the ABI
 *   - Mesa vulkan wsi_display     (not in our build path, still satisfied)
 *   libdrm was checked and does NOT call libudev (it reads sysfs directly), so
 *   Mesa's GBM/EGL path needs synthetic sysfs, not this shim.
 *
 * Memory model: reference-counted contexts/devices/enumerators/monitors. All
 * device strings are static literals (no per-device string ownership); only
 * the small list nodes and the device/enumerate wrappers are heap-allocated.
 */

#include "libudev.h"

#include <errno.h>
#include <fnmatch.h>
#include <stdarg.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <pthread.h>

#include <sys/ioctl.h>

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>

/* ===================================================================== */
/* Self-tracing (LEANDROS_INPUT_TRACE)                                    */
/*                                                                         */
/* Every traced call is tagged "[UDEVSHIM] pid=<pid> ..." so that the      */
/* interleaved stderr of cosmic-comp/cosmic-panel/cosmic-settings-daemon/  */
/* Mesa (all of which load this shim) can be attributed line by line.     */
/* Disabled unless LEANDROS_INPUT_TRACE is set to something other than    */
/* "" or "0"; the getenv() result is cached so it is read exactly once.   */
/* Hard-capped at TRACE_BUDGET lines total so a runaway consumer (e.g. a  */
/* monitor-receive poll loop) cannot flood the guest console.             */
/* trc() saves/restores errno around its own work so tracing is provably  */
/* invisible to callers that inspect errno after a shim call.             */
/* Mirrored to LEANDROS_INPUT_TRACE_DIR/udevshim.<pid>.log (if set) since */
/* stderr alone is not reliable: cosmic-session may repoint fd 2 out from */
/* under a child it launches; see trace_file() below.                    */
/* ===================================================================== */

#define TRACE_BUDGET 600

static int trace_on(void) {
	static int cached = -1;
	if (cached < 0) {
		const char *v = getenv("LEANDROS_INPUT_TRACE");
		cached = (v != NULL && v[0] != '\0' && strcmp(v, "0") != 0) ? 1 : 0;
	}
	return cached;
}

#define TRACE_FILE_FAILED ((FILE *)-1)

/* Second sink: LEANDROS_INPUT_TRACE_DIR, if set to a non-empty directory
 * path, gets a per-process file "<dir>/udevshim.<pid>.log" in addition to
 * stderr. Opened lazily on the first traced line; on open failure we fall
 * back to stderr-only and never retry -- TRACE_FILE_FAILED is the sentinel
 * that makes that permanent without re-calling fopen() on every line. */
static FILE *trace_file(void) {
	static FILE *f = NULL;
	if (f == TRACE_FILE_FAILED) return NULL;
	if (f != NULL) return f;

	const char *dir = getenv("LEANDROS_INPUT_TRACE_DIR");
	if (dir == NULL || dir[0] == '\0') {
		f = TRACE_FILE_FAILED;
		return NULL;
	}

	char path[PATH_MAX];
	int n = snprintf(path, sizeof(path), "%s/udevshim.%d.log", dir, (int)getpid());
	if (n < 0 || (size_t)n >= sizeof(path)) {
		f = TRACE_FILE_FAILED;
		return NULL;
	}

	FILE *fp = fopen(path, "ae"); /* 'e' -> O_CLOEXEC, no fd leak across exec */
	if (fp == NULL) {
		f = TRACE_FILE_FAILED;
		return NULL;
	}
	setvbuf(fp, NULL, _IOLBF, 0);
	f = fp;
	return f;
}

static void trc(const char *fmt, ...) {
	static unsigned long n_lines = 0;
	if (!trace_on()) return;
	int saved_errno = errno;
	if (n_lines >= TRACE_BUDGET) {
		errno = saved_errno;
		return;
	}
	n_lines++;

	char msg[1024];
	va_list ap;
	va_start(ap, fmt);
	vsnprintf(msg, sizeof(msg), fmt, ap);
	va_end(ap);

	char line[1024 + 64];
	snprintf(line, sizeof(line), "[UDEVSHIM] pid=%d %s\n", (int)getpid(), msg);

	fputs(line, stderr);
	fflush(stderr);
	FILE *tf = trace_file();
	if (tf != NULL) {
		fputs(line, tf);
		fflush(tf);
	}

	if (n_lines == TRACE_BUDGET) {
		char exhausted[128];
		snprintf(exhausted, sizeof(exhausted),
		         "[UDEVSHIM] pid=%d TRACE BUDGET EXHAUSTED after %d lines\n",
		         (int)getpid(), TRACE_BUDGET);
		fputs(exhausted, stderr);
		fflush(stderr);
		if (tf != NULL) {
			fputs(exhausted, tf);
			fflush(tf);
		}
	}

	errno = saved_errno;
}

/* ===================================================================== */
/* Static device model                                                    */
/* ===================================================================== */

struct prop {
	const char *key;
	const char *val;
};

struct dev_desc {
	const char *syspath;
	const char *sysname;
	const char *sysnum;    /* trailing number as a string, or NULL */
	const char *subsystem; /* "drm" | "input" | "platform" */
	const char *devtype;   /* usually NULL for these nodes */
	const char *devnode;   /* /dev/... or NULL for non-device parents */
	const char *driver;    /* or NULL */
	char dev_kind;         /* 'c' char, 'b' block, 0 = no devnum */
	unsigned major;
	unsigned minor;
	int is_initialized;
	const char *parent_syspath; /* or NULL */
	const struct prop *props;
	size_t nprops;
};

/* --- property tables (data-driven; edit here to reshape the device set) --- */

static const struct prop props_card0[] = {
	{ "DEVNAME", "/dev/dri/card0" },
	{ "MAJOR", "226" },
	{ "MINOR", "0" },
	{ "ID_SEAT", "seat0" },
};
static const struct prop props_render[] = {
	{ "DEVNAME", "/dev/dri/renderD128" },
	{ "MAJOR", "226" },
	{ "MINOR", "128" },
};
#define NPROPS(a) (sizeof(a) / sizeof((a)[0]))

/*
 * The device table. Order matters only for stable enumeration output.
 * Parents (input<N>, the platform GPU) are listed so that get_parent() and
 * new_from_syspath() resolve them, but they carry no devnode.
 */
static const struct dev_desc g_devices[] = {
	/* --- drm --- */
	{ "/sys/devices/platform/gpu", "gpu", NULL, "platform", NULL, NULL, "virtio_gpu",
	  0, 0, 0, 1, NULL, NULL, 0 },
	{ "/sys/class/drm/card0", "card0", "0", "drm", NULL, "/dev/dri/card0", NULL,
	  'c', 226, 0, 1, "/sys/devices/platform/gpu", props_card0, NPROPS(props_card0) },
	{ "/sys/class/drm/renderD128", "renderD128", "128", "drm", NULL, "/dev/dri/renderD128", NULL,
	  'c', 226, 128, 1, "/sys/devices/platform/gpu", props_render, NPROPS(props_render) },

};
static const size_t g_ndevices = sizeof(g_devices) / sizeof(g_devices[0]);

/* ===================================================================== */
/* Dynamic input model: /dev/input/eventN as the evdev server has them     */
/* ===================================================================== */
/*
 * The kernel's /dev/input/hotplug node (servers/evdev) is a uevent-like
 * stream: each open is seeded with one "add eventN kind bus vid pid" record
 * per present device, then gets "add"/"remove" records as devices come and
 * go; one read(2) returns one record. Enumeration opens it, reads the
 * coldplug records and closes it; a monitor keeps it open. Each device's
 * udev properties come from its own capabilities (EVIOCGBIT/EVIOCGPROP/
 * EVIOCGID/EVIOCGNAME, as udev's input_id builtin does); a node this process
 * cannot open (0660 root:input) falls back to the record's kind.
 */
#define LX_IOC_READ 2u
#define LX_IOC(dir, type, nr, size) ((unsigned)(((dir) << 30) | ((size) << 16) | ((type) << 8) | (nr)))
#define LX_EVIOCGID LX_IOC(LX_IOC_READ, 'E', 0x02, 8u)
#define LX_EVIOCGNAME(len) LX_IOC(LX_IOC_READ, 'E', 0x06, (unsigned)(len))
#define LX_EVIOCGPROP(len) LX_IOC(LX_IOC_READ, 'E', 0x09, (unsigned)(len))
#define LX_EVIOCGBIT(ev, len) LX_IOC(LX_IOC_READ, 'E', 0x20 + (ev), (unsigned)(len))

#define MAX_INPUTS 32
#define HOTPLUG_NODE "/dev/input/hotplug"

enum { C_KEYBOARD = 1, C_KEY = 2, C_MOUSE = 4, C_TABLET = 8, C_TOUCHPAD = 16,
       C_TOUCHSCREEN = 32, C_JOYSTICK = 64 };

struct input_info {
	int present;
	unsigned cls;
	unsigned bus, vendor, product;
	char name[80];
};
static struct input_info g_inputs[MAX_INPUTS];
static int g_inputs_scanned;
static pthread_mutex_t g_inputs_lock = PTHREAD_MUTEX_INITIALIZER;

static int bit_set(const unsigned char *b, size_t len, unsigned bit) {
	return bit / 8 < len && ((b[bit / 8] >> (bit % 8)) & 1);
}

static unsigned cls_from_kind(const char *kind) {
	if (strcmp(kind, "kbd") == 0) return C_KEYBOARD | C_KEY;
	if (strcmp(kind, "touchpad") == 0) return C_TOUCHPAD;
	return C_MOUSE; /* "mouse", and "tablet" = virtio-tablet absolute pointer */
}

/* The part of udev's input_id builtin that libinput reads. */
static unsigned cls_from_caps(int fd) {
	unsigned char ev[4] = {0}, key[96] = {0}, rel[4] = {0}, abs[8] = {0}, prop[4] = {0};
	if (ioctl(fd, (int)LX_EVIOCGBIT(0, sizeof ev), ev) < 0) return 0;
	ioctl(fd, (int)LX_EVIOCGBIT(1, sizeof key), key);
	ioctl(fd, (int)LX_EVIOCGBIT(2, sizeof rel), rel);
	ioctl(fd, (int)LX_EVIOCGBIT(3, sizeof abs), abs);
	ioctl(fd, (int)LX_EVIOCGPROP(sizeof prop), prop); /* may be unsupported: stays 0 */
	unsigned c = 0;
	if (bit_set(ev, sizeof ev, 1)) {
		int full = 1;
		for (unsigned k = 1; k < 32; k++)
			if (!bit_set(key, sizeof key, k)) { full = 0; break; }
		if (full) c |= C_KEYBOARD;
		for (unsigned k = 1; k < 0x100; k++)
			if (bit_set(key, sizeof key, k)) { c |= C_KEY; break; }
	}
	int btn_left = bit_set(key, sizeof key, 0x110);
	int pen = bit_set(key, sizeof key, 0x140) || bit_set(key, sizeof key, 0x14b);
	int finger = bit_set(key, sizeof key, 0x145);
	int touch = bit_set(key, sizeof key, 0x14a);
	int joy = bit_set(key, sizeof key, 0x120) || bit_set(key, sizeof key, 0x130);
	int direct = bit_set(prop, sizeof prop, 1);
	if (bit_set(ev, sizeof ev, 2) && bit_set(rel, sizeof rel, 0) && bit_set(rel, sizeof rel, 1) && btn_left)
		c |= C_MOUSE;
	if (bit_set(ev, sizeof ev, 3) && bit_set(abs, sizeof abs, 0) && bit_set(abs, sizeof abs, 1)) {
		if (pen) c |= C_TABLET;
		else if (finger && !direct) c |= C_TOUCHPAD;
		else if (touch || direct) c |= C_TOUCHSCREEN;
		else if (btn_left) c |= C_MOUSE;
		else if (joy) c |= C_JOYSTICK;
	}
	return c;
}

static void probe_input(struct input_info *in, int n, const char *kind,
                        unsigned bus, unsigned vendor, unsigned product) {
	memset(in, 0, sizeof *in);
	in->present = 1;
	in->bus = bus; in->vendor = vendor; in->product = product;
	char path[32];
	snprintf(path, sizeof path, "/dev/input/event%d", n);
	int fd = open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
	if (fd >= 0) {
		in->cls = cls_from_caps(fd);
		unsigned short id[4];
		if (ioctl(fd, (int)LX_EVIOCGID, id) == 0) {
			in->bus = id[0]; in->vendor = id[1]; in->product = id[2];
		}
		if (ioctl(fd, (int)LX_EVIOCGNAME(sizeof in->name - 1), in->name) < 0) in->name[0] = 0;
		in->name[sizeof in->name - 1] = 0;
		close(fd);
	}
	if (!in->cls) in->cls = cls_from_kind(kind);
	trc("input_probe event%d kind=%s fd=%d cls=0x%x name=%s", n, kind, fd, in->cls, in->name);
}

/* "add event5 kbd 0003 0627 0001" -> fields; 0 when malformed. */
static int parse_record(const char *line, char act[8], int *n, char kind[8],
                        unsigned *bus, unsigned *vid, unsigned *pid) {
	if (sscanf(line, "%7s event%d %7s %x %x %x", act, n, kind, bus, vid, pid) != 6) return 0;
	return *n >= 0 && *n < MAX_INPUTS;
}

static void inputs_rescan_locked(void) {
	memset(g_inputs, 0, sizeof g_inputs);
	g_inputs_scanned = 1;
	int fd = open(HOTPLUG_NODE, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
	if (fd < 0) { /* kernel without the hotplug node: its two fixed nodes */
		probe_input(&g_inputs[0], 0, "kbd", 0, 0, 0);
		probe_input(&g_inputs[1], 1, "tablet", 0, 0, 0);
		return;
	}
	char line[96];
	ssize_t r;
	for (int i = 0; i < 4 * MAX_INPUTS && (r = read(fd, line, sizeof line - 1)) > 0; i++) {
		line[r] = 0;
		char act[8], kind[8]; int n; unsigned b, v, p;
		if (parse_record(line, act, &n, kind, &b, &v, &p) && strcmp(act, "add") == 0)
			probe_input(&g_inputs[n], n, kind, b, v, p);
	}
	close(fd);
}

struct dyn_desc {
	struct dev_desc d;
	struct prop props[20];
	char syspath[48], sysname[16], sysnum[8], devnode[32], parent[48];
	char minor[8], name[96], vid[8], pid[8], product[40];
};

static struct dyn_desc *dyn_input(int n, int is_parent, const struct input_info *in) {
	struct dyn_desc *x = calloc(1, sizeof *x);
	if (!x) return NULL;
	size_t k = 0;
#define DP(K, V) do { x->props[k].key = (K); x->props[k].val = (V); k++; } while (0)
	snprintf(x->sysnum, sizeof x->sysnum, "%d", n);
	snprintf(x->parent, sizeof x->parent, "/sys/class/input/input%d", n);
	if (is_parent) {
		snprintf(x->syspath, sizeof x->syspath, "/sys/class/input/input%d", n);
		snprintf(x->sysname, sizeof x->sysname, "input%d", n);
	} else {
		snprintf(x->syspath, sizeof x->syspath, "/sys/class/input/event%d", n);
		snprintf(x->sysname, sizeof x->sysname, "event%d", n);
		snprintf(x->devnode, sizeof x->devnode, "/dev/input/event%d", n);
		snprintf(x->minor, sizeof x->minor, "%d", 64 + n);
		DP("DEVNAME", x->devnode);
		DP("MAJOR", "13");
		DP("MINOR", x->minor);
	}
	DP("ID_INPUT", "1");
	if (in->cls & C_KEYBOARD) DP("ID_INPUT_KEYBOARD", "1");
	if (in->cls & C_KEY) DP("ID_INPUT_KEY", "1");
	if (in->cls & C_MOUSE) DP("ID_INPUT_MOUSE", "1");
	if (in->cls & C_TABLET) DP("ID_INPUT_TABLET", "1");
	if (in->cls & C_TOUCHPAD) DP("ID_INPUT_TOUCHPAD", "1");
	if (in->cls & C_TOUCHSCREEN) DP("ID_INPUT_TOUCHSCREEN", "1");
	if (in->cls & C_JOYSTICK) DP("ID_INPUT_JOYSTICK", "1");
	DP("ID_SEAT", "seat0");
	if (in->name[0]) {
		snprintf(x->name, sizeof x->name, "\"%s\"", in->name);
		DP("NAME", x->name);
	}
	if (in->bus == 3) DP("ID_BUS", "usb");
	snprintf(x->vid, sizeof x->vid, "%04x", in->vendor);
	snprintf(x->pid, sizeof x->pid, "%04x", in->product);
	snprintf(x->product, sizeof x->product, "%x/%x/%x/0", in->bus, in->vendor, in->product);
	DP("ID_VENDOR_ID", x->vid);
	DP("ID_MODEL_ID", x->pid);
	DP("PRODUCT", x->product);
#undef DP
	x->d.syspath = x->syspath;
	x->d.sysname = x->sysname;
	x->d.sysnum = x->sysnum;
	x->d.subsystem = "input";
	x->d.devnode = is_parent ? NULL : x->devnode;
	x->d.dev_kind = is_parent ? 0 : 'c';
	x->d.major = is_parent ? 0 : 13;
	x->d.minor = is_parent ? 0 : 64 + (unsigned)n;
	x->d.is_initialized = 1;
	x->d.parent_syspath = is_parent ? NULL : x->parent;
	x->d.props = x->props;
	x->d.nprops = k;
	return x;
}

/* Input node n (event node, or its inputN parent) when present. */
static struct dyn_desc *input_lookup(int n, int is_parent) {
	if (n < 0 || n >= MAX_INPUTS) return NULL;
	pthread_mutex_lock(&g_inputs_lock);
	if (!g_inputs_scanned) inputs_rescan_locked();
	struct input_info in = g_inputs[n];
	pthread_mutex_unlock(&g_inputs_lock);
	return in.present ? dyn_input(n, is_parent, &in) : NULL;
}

/* "event5" / "input5" -> (5, is_parent); -1 otherwise. */
static int input_sysname(const char *s, int *is_parent) {
	int n; char tail;
	if (sscanf(s, "event%d%c", &n, &tail) == 1) { *is_parent = 0; return n; }
	if (sscanf(s, "input%d%c", &n, &tail) == 1) { *is_parent = 1; return n; }
	return -1;
}

static const struct dev_desc *desc_by_syspath(const char *syspath, struct dyn_desc **own) {
	*own = NULL;
	if (!syspath) return NULL;
	if (strncmp(syspath, "/sys/class/input/", 17) == 0) {
		int par, n = input_sysname(syspath + 17, &par);
		*own = n >= 0 ? input_lookup(n, par) : NULL;
		return *own ? &(*own)->d : NULL;
	}
	for (size_t i = 0; i < g_ndevices; i++)
		if (strcmp(g_devices[i].syspath, syspath) == 0)
			return &g_devices[i];
	return NULL;
}

static const struct dev_desc *desc_by_devnum(char kind, dev_t num, struct dyn_desc **own) {
	unsigned ma = major(num), mi = minor(num);
	*own = NULL;
	if (kind == 'c' && ma == 13 && mi >= 64) {
		*own = input_lookup((int)mi - 64, 0);
		return *own ? &(*own)->d : NULL;
	}
	for (size_t i = 0; i < g_ndevices; i++) {
		const struct dev_desc *d = &g_devices[i];
		if (d->dev_kind == kind && d->major == ma && d->minor == mi)
			return d;
	}
	return NULL;
}

static const struct dev_desc *desc_by_subsystem_sysname(const char *subsystem, const char *sysname,
                                                        struct dyn_desc **own) {
	*own = NULL;
	if (!subsystem || !sysname) return NULL;
	if (strcmp(subsystem, "input") == 0) {
		int par, n = input_sysname(sysname, &par);
		*own = n >= 0 ? input_lookup(n, par) : NULL;
		return *own ? &(*own)->d : NULL;
	}
	for (size_t i = 0; i < g_ndevices; i++) {
		const struct dev_desc *d = &g_devices[i];
		if (strcmp(d->subsystem, subsystem) == 0 && strcmp(d->sysname, sysname) == 0)
			return d;
	}
	return NULL;
}

/* ===================================================================== */
/* Generic list (udev_list_entry)                                         */
/* ===================================================================== */

struct udev_list_entry {
	char *name;
	char *value;
	struct udev_list_entry *next;
};

static void list_free(struct udev_list_entry *e) {
	while (e) {
		struct udev_list_entry *n = e->next;
		free(e->name);
		free(e->value);
		free(e);
		e = n;
	}
}

/* Append (name,value); value may be NULL. Returns new head via *head. */
static int list_append(struct udev_list_entry **head, const char *name, const char *value) {
	struct udev_list_entry *e = calloc(1, sizeof(*e));
	if (!e) return -ENOMEM;
	e->name = name ? strdup(name) : NULL;
	e->value = value ? strdup(value) : NULL;
	if ((name && !e->name) || (value && !e->value)) {
		free(e->name);
		free(e->value);
		free(e);
		return -ENOMEM;
	}
	if (!*head) {
		*head = e;
	} else {
		struct udev_list_entry *t = *head;
		while (t->next) t = t->next;
		t->next = e;
	}
	return 0;
}

struct udev_list_entry *udev_list_entry_get_next(struct udev_list_entry *e) {
	return e ? e->next : NULL;
}

struct udev_list_entry *udev_list_entry_get_by_name(struct udev_list_entry *e, const char *name) {
	for (; e; e = e->next)
		if (e->name && name && strcmp(e->name, name) == 0)
			return e;
	return NULL;
}

const char *udev_list_entry_get_name(struct udev_list_entry *e) {
	return e ? e->name : NULL;
}

const char *udev_list_entry_get_value(struct udev_list_entry *e) {
	return e ? e->value : NULL;
}

/* ===================================================================== */
/* udev context                                                           */
/* ===================================================================== */

struct udev {
	int refcount;
	void *userdata;
	int log_priority;
};

struct udev *udev_new(void) {
	struct udev *u = calloc(1, sizeof(*u));
	if (!u) {
		errno = ENOMEM;
		return NULL;
	}
	u->refcount = 1;
	u->log_priority = 3; /* LOG_ERR */
	trc("udev_new ctx=%p", (void *)u);
	return u;
}

struct udev *udev_ref(struct udev *u) {
	if (u) u->refcount++;
	return u;
}

struct udev *udev_unref(struct udev *u) {
	if (u && --u->refcount <= 0)
		free(u);
	return NULL;
}

void *udev_get_userdata(struct udev *u) {
	return u ? u->userdata : NULL;
}

void udev_set_userdata(struct udev *u, void *userdata) {
	if (u) u->userdata = userdata;
}

/* deprecated logging controls: accepted, no-op */
void udev_set_log_fn(struct udev *u,
                     void (*log_fn)(struct udev *, int, const char *, int, const char *, const char *, va_list)) {
	(void)u;
	(void)log_fn;
}
int udev_get_log_priority(struct udev *u) {
	return u ? u->log_priority : 3;
}
void udev_set_log_priority(struct udev *u, int priority) {
	if (u) u->log_priority = priority;
}

/* ===================================================================== */
/* udev_device                                                            */
/* ===================================================================== */

struct udev_device {
	int refcount;
	struct udev *udev;
	const struct dev_desc *desc;
	struct udev_device *parent; /* lazily created, owned, unref'd with us */
	struct udev_list_entry *properties;
	struct udev_list_entry *sysattrs;
	struct udev_list_entry *devlinks;
	int properties_built;
	struct dyn_desc *own;  /* heap descriptor of an input node, freed with us */
	char action[8];        /* "add"/"remove" for a monitor event, else "" */
};

static struct udev_device *device_wrap(struct udev *udev, const struct dev_desc *desc, struct dyn_desc *own) {
	if (!desc) { free(own); return NULL; }
	struct udev_device *d = calloc(1, sizeof(*d));
	if (!d) {
		free(own);
		errno = ENOMEM;
		return NULL;
	}
	d->own = own;
	d->refcount = 1;
	d->udev = udev_ref(udev);
	d->desc = desc;
	return d;
}

struct udev_device *udev_device_ref(struct udev_device *d) {
	if (d) d->refcount++;
	return d;
}

struct udev_device *udev_device_unref(struct udev_device *d) {
	if (!d) return NULL;
	if (--d->refcount > 0) return NULL;
	if (d->parent) {
		/* parent's refcount is independent; drop our reference */
		udev_device_unref(d->parent);
	}
	list_free(d->properties);
	list_free(d->sysattrs);
	list_free(d->devlinks);
	udev_unref(d->udev);
	free(d->own);
	free(d);
	return NULL;
}

struct udev *udev_device_get_udev(struct udev_device *d) {
	return d ? d->udev : NULL;
}

struct udev_device *udev_device_new_from_syspath(struct udev *udev, const char *syspath) {
	struct dyn_desc *own;
	const struct dev_desc *desc = desc_by_syspath(syspath, &own);
	struct udev_device *d = NULL;
	if (!desc) {
		errno = ENODEV;
	} else {
		d = device_wrap(udev, desc, own);
	}
	trc("dev_from_syspath path=%s -> dev=%p", syspath ? syspath : "(null)", (void *)d);
	return d;
}

struct udev_device *udev_device_new_from_devnum(struct udev *udev, char type, dev_t devnum) {
	struct dyn_desc *own;
	const struct dev_desc *desc = desc_by_devnum(type, devnum, &own);
	struct udev_device *d = NULL;
	if (!desc) {
		errno = ENODEV;
	} else {
		d = device_wrap(udev, desc, own);
	}
	trc("dev_from_devnum type=%c major=%u minor=%u -> dev=%p",
	    type, (unsigned)major(devnum), (unsigned)minor(devnum), (void *)d);
	return d;
}

struct udev_device *udev_device_new_from_subsystem_sysname(struct udev *udev, const char *subsystem, const char *sysname) {
	struct dyn_desc *own;
	const struct dev_desc *desc = desc_by_subsystem_sysname(subsystem, sysname, &own);
	struct udev_device *d = NULL;
	if (!desc) {
		errno = ENODEV;
	} else {
		d = device_wrap(udev, desc, own);
	}
	trc("dev_from_subsystem_sysname sub=%s name=%s -> dev=%p",
	    subsystem ? subsystem : "(null)", sysname ? sysname : "(null)", (void *)d);
	return d;
}

struct udev_device *udev_device_new_from_device_id(struct udev *udev, const char *id) {
	/* id form: "c226:0" / "b8:0" / "+subsystem:sysname". Support the
	 * char/block numeric form used in practice; others -> ENODEV. */
	if (!id || (id[0] != 'c' && id[0] != 'b')) {
		errno = EINVAL;
		trc("dev_from_device_id id=%s -> dev=(nil)", id ? id : "(null)");
		return NULL;
	}
	char kind = id[0];
	unsigned ma = 0, mi = 0;
	if (sscanf(id + 1, "%u:%u", &ma, &mi) != 2) {
		errno = EINVAL;
		trc("dev_from_device_id id=%s -> dev=(nil)", id);
		return NULL;
	}
	struct udev_device *d = udev_device_new_from_devnum(udev, kind, makedev(ma, mi));
	trc("dev_from_device_id id=%s -> dev=%p", id, (void *)d);
	return d;
}

struct udev_device *udev_device_new_from_environment(struct udev *udev) {
	(void)udev;
	errno = ENODEV; /* no uevent environment on LeandrOS */
	return NULL;
}

struct udev_device *udev_device_get_parent(struct udev_device *d) {
	if (!d) return NULL;
	struct udev_device *result;
	if (d->parent) {
		result = d->parent;
	} else if (!d->desc->parent_syspath) {
		result = NULL;
	} else {
		struct dyn_desc *own;
		const struct dev_desc *pd = desc_by_syspath(d->desc->parent_syspath, &own);
		result = pd ? device_wrap(d->udev, pd, own) : NULL;
		d->parent = result; /* owned by child, not an extra ref for the caller */
	}
	trc("dev_parent syspath=%s sub=%s devtype=%s -> %s",
	    d->desc->syspath, "-", "-", result ? result->desc->syspath : "(null)");
	return result;
}

struct udev_device *udev_device_get_parent_with_subsystem_devtype(struct udev_device *d,
                                                                  const char *subsystem,
                                                                  const char *devtype) {
	struct udev_device *p = udev_device_get_parent(d);
	while (p) {
		const char *psub = p->desc->subsystem;
		const char *pdt = p->desc->devtype;
		int sub_ok = (!subsystem) || (psub && strcmp(psub, subsystem) == 0);
		int dt_ok = (!devtype) || (pdt && strcmp(pdt, devtype) == 0);
		if (sub_ok && dt_ok) {
			trc("dev_parent syspath=%s sub=%s devtype=%s -> %s",
			    d ? d->desc->syspath : "(null)",
			    subsystem ? subsystem : "(null)", devtype ? devtype : "(null)",
			    p->desc->syspath);
			return p;
		}
		p = udev_device_get_parent(p);
	}
	trc("dev_parent syspath=%s sub=%s devtype=%s -> %s",
	    d ? d->desc->syspath : "(null)",
	    subsystem ? subsystem : "(null)", devtype ? devtype : "(null)", "(null)");
	return NULL;
}

const char *udev_device_get_devpath(struct udev_device *d) {
	/* devpath is the syspath with the leading "/sys" stripped */
	if (!d) return NULL;
	const char *sp = d->desc->syspath;
	if (strncmp(sp, "/sys", 4) == 0) return sp + 4;
	return sp;
}
const char *udev_device_get_subsystem(struct udev_device *d) { return d ? d->desc->subsystem : NULL; }
const char *udev_device_get_devtype(struct udev_device *d) { return d ? d->desc->devtype : NULL; }
const char *udev_device_get_syspath(struct udev_device *d) { return d ? d->desc->syspath : NULL; }
const char *udev_device_get_sysname(struct udev_device *d) { return d ? d->desc->sysname : NULL; }
const char *udev_device_get_sysnum(struct udev_device *d) { return d ? d->desc->sysnum : NULL; }
const char *udev_device_get_devnode(struct udev_device *d) {
	const char *node = d ? d->desc->devnode : NULL;
	trc("dev_devnode dev=%p syspath=%s -> %s",
	    (void *)d, d ? d->desc->syspath : "(null)", node ? node : "(null)");
	return node;
}
const char *udev_device_get_driver(struct udev_device *d) { return d ? d->desc->driver : NULL; }
const char *udev_device_get_action(struct udev_device *d) {
	return d && d->action[0] ? d->action : NULL; /* set only on monitor events */
}

int udev_device_get_is_initialized(struct udev_device *d) {
	int r = d ? d->desc->is_initialized : 0;
	trc("dev_is_init syspath=%s -> %d", d ? d->desc->syspath : "(null)", r);
	return r;
}

dev_t udev_device_get_devnum(struct udev_device *d) {
	if (!d || d->desc->dev_kind == 0) return makedev(0, 0);
	return makedev(d->desc->major, d->desc->minor);
}

unsigned long long int udev_device_get_seqnum(struct udev_device *d) { (void)d; return 0; }
unsigned long long int udev_device_get_usec_since_initialized(struct udev_device *d) { (void)d; return 0; }

static void device_build_properties(struct udev_device *d) {
	if (d->properties_built) return;
	d->properties_built = 1;
	for (size_t i = 0; i < d->desc->nprops; i++)
		list_append(&d->properties, d->desc->props[i].key, d->desc->props[i].val);
}

struct udev_list_entry *udev_device_get_properties_list_entry(struct udev_device *d) {
	if (!d) {
		trc("dev_properties_list dev=(nil) syspath=(null) head=(nil)");
		return NULL;
	}
	device_build_properties(d);
	trc("dev_properties_list dev=%p syspath=%s head=%p",
	    (void *)d, d->desc->syspath, (void *)d->properties);
	return d->properties;
}

const char *udev_device_get_property_value(struct udev_device *d, const char *key) {
	if (!d || !key) {
		trc("dev_prop syspath=%s key=%s -> %s",
		    d ? d->desc->syspath : "(null)", key ? key : "(null)", "(null)");
		return NULL;
	}
	const char *val = NULL;
	for (size_t i = 0; i < d->desc->nprops; i++)
		if (strcmp(d->desc->props[i].key, key) == 0) {
			val = d->desc->props[i].val;
			break;
		}
	trc("dev_prop syspath=%s key=%s -> %s", d->desc->syspath, key, val ? val : "(null)");
	return val;
}

struct udev_list_entry *udev_device_get_devlinks_list_entry(struct udev_device *d) {
	(void)d;
	return NULL; /* no /dev/input/by-id or by-path symlinks modeled */
}

struct udev_list_entry *udev_device_get_tags_list_entry(struct udev_device *d) { (void)d; return NULL; }
struct udev_list_entry *udev_device_get_current_tags_list_entry(struct udev_device *d) { (void)d; return NULL; }
struct udev_list_entry *udev_device_get_sysattr_list_entry(struct udev_device *d) { (void)d; return NULL; }

const char *udev_device_get_sysattr_value(struct udev_device *d, const char *sysattr) {
	trc("dev_sysattr syspath=%s attr=%s -> %s",
	    d ? d->desc->syspath : "(null)", sysattr ? sysattr : "(null)", "(null)");
	return NULL; /* sysattrs served later off synthetic sysfs */
}

int udev_device_set_sysattr_value(struct udev_device *d, const char *sysattr, const char *value) {
	(void)d;
	(void)sysattr;
	(void)value;
	errno = EROFS; /* read-only device model */
	return -1;
}

int udev_device_has_tag(struct udev_device *d, const char *tag) { (void)d; (void)tag; return 0; }
int udev_device_has_current_tag(struct udev_device *d, const char *tag) { (void)d; (void)tag; return 0; }

/* ===================================================================== */
/* udev_enumerate                                                         */
/* ===================================================================== */

#define MAX_FILTERS 16

struct udev_enumerate {
	int refcount;
	struct udev *udev;
	/* subsystem match/nomatch filters */
	const char *match_subsystem[MAX_FILTERS];
	size_t n_match_subsystem;
	const char *nomatch_subsystem[MAX_FILTERS];
	size_t n_nomatch_subsystem;
	/* sysname match */
	const char *match_sysname[MAX_FILTERS];
	size_t n_match_sysname;
	/* property match (key,val) */
	const char *match_prop_key[MAX_FILTERS];
	const char *match_prop_val[MAX_FILTERS];
	size_t n_match_prop;
	struct udev_list_entry *results;
};

struct udev_enumerate *udev_enumerate_new(struct udev *udev) {
	struct udev_enumerate *e = calloc(1, sizeof(*e));
	if (!e) {
		errno = ENOMEM;
		return NULL;
	}
	e->refcount = 1;
	e->udev = udev_ref(udev);
	trc("enum_new e=%p", (void *)e);
	return e;
}

struct udev_enumerate *udev_enumerate_ref(struct udev_enumerate *e) {
	if (e) e->refcount++;
	return e;
}

struct udev_enumerate *udev_enumerate_unref(struct udev_enumerate *e) {
	if (!e) return NULL;
	if (--e->refcount > 0) return NULL;
	/* Free the strdup'd match strings (see add_match_* setters). */
	for (size_t i = 0; i < e->n_match_subsystem; i++) free((void *)e->match_subsystem[i]);
	for (size_t i = 0; i < e->n_nomatch_subsystem; i++) free((void *)e->nomatch_subsystem[i]);
	for (size_t i = 0; i < e->n_match_sysname; i++) free((void *)e->match_sysname[i]);
	for (size_t i = 0; i < e->n_match_prop; i++) {
		free((void *)e->match_prop_key[i]);
		free((void *)e->match_prop_val[i]);
	}
	list_free(e->results);
	udev_unref(e->udev);
	free(e);
	return NULL;
}

struct udev *udev_enumerate_get_udev(struct udev_enumerate *e) { return e ? e->udev : NULL; }

/* Match-string setters MUST copy: libudev owns the strings past the call, and
 * callers routinely pass a temporary (e.g. the Rust `udev` crate hands us a
 * CString that it drops the instant add_match_* returns). Storing the caller's
 * pointer left us dereferencing freed memory at scan time — the subsystem/
 * sysname compared as empty/garbage, so a Rust-side `match_subsystem("drm")` +
 * `match_sysname("card[0-9]*")` matched nothing while a C caller passing string
 * literals (libinput) worked. strdup here (freed in udev_enumerate_unref). */
int udev_enumerate_add_match_subsystem(struct udev_enumerate *e, const char *subsystem) {
	if (!e || !subsystem) return -EINVAL;
	trc("enum_match_subsystem e=%p sub=%s", (void *)e, subsystem);
	if (e->n_match_subsystem < MAX_FILTERS) {
		char *c = strdup(subsystem);
		if (!c) return -ENOMEM;
		e->match_subsystem[e->n_match_subsystem++] = c;
	}
	return 0;
}
int udev_enumerate_add_nomatch_subsystem(struct udev_enumerate *e, const char *subsystem) {
	if (!e || !subsystem) return -EINVAL;
	/* NOTE: distinct tag from add_match_subsystem so a log reader can tell
	 * match from nomatch apart; the task spec bundled both under one line
	 * shape, but a shared tag would make the two indistinguishable. */
	trc("enum_nomatch_subsystem e=%p sub=%s", (void *)e, subsystem);
	if (e->n_nomatch_subsystem < MAX_FILTERS) {
		char *c = strdup(subsystem);
		if (!c) return -ENOMEM;
		e->nomatch_subsystem[e->n_nomatch_subsystem++] = c;
	}
	return 0;
}
int udev_enumerate_add_match_sysname(struct udev_enumerate *e, const char *sysname) {
	if (!e || !sysname) return -EINVAL;
	trc("enum_match_sysname e=%p name=%s", (void *)e, sysname);
	if (e->n_match_sysname < MAX_FILTERS) {
		char *c = strdup(sysname);
		if (!c) return -ENOMEM;
		e->match_sysname[e->n_match_sysname++] = c;
	}
	return 0;
}
int udev_enumerate_add_match_property(struct udev_enumerate *e, const char *property, const char *value) {
	if (!e || !property) return -EINVAL;
	trc("enum_match_property e=%p key=%s val=%s", (void *)e, property, value ? value : "(null)");
	if (e->n_match_prop < MAX_FILTERS) {
		char *k = strdup(property);
		char *v = value ? strdup(value) : NULL;
		if (!k || (value && !v)) { free(k); free(v); return -ENOMEM; }
		e->match_prop_key[e->n_match_prop] = k;
		e->match_prop_val[e->n_match_prop] = v;
		e->n_match_prop++;
	}
	return 0;
}
/* Accepted-but-not-applied filters (documented): our device set is tiny and
 * fixed, so these never change the result for real consumers. */
int udev_enumerate_add_match_sysattr(struct udev_enumerate *e, const char *sysattr, const char *value) {
	(void)e; (void)sysattr; (void)value; return 0;
}
int udev_enumerate_add_nomatch_sysattr(struct udev_enumerate *e, const char *sysattr, const char *value) {
	(void)e; (void)sysattr; (void)value; return 0;
}
int udev_enumerate_add_match_tag(struct udev_enumerate *e, const char *tag) {
	trc("enum_match_tag e=%p tag=%s", (void *)e, tag ? tag : "(null)");
	return 0;
}
int udev_enumerate_add_match_parent(struct udev_enumerate *e, struct udev_device *parent) {
	trc("enum_match_parent e=%p parent=%p", (void *)e, (void *)parent);
	return 0;
}
int udev_enumerate_add_match_is_initialized(struct udev_enumerate *e) {
	trc("enum_match_is_initialized e=%p", (void *)e);
	return 0; /* everything is initialized */
}
/* Not in the requested list, but clearly on the enumeration path (an
 * explicit-syspath seed) so it is traced too. */
int udev_enumerate_add_syspath(struct udev_enumerate *e, const char *syspath) {
	if (!e || !syspath) return -EINVAL;
	struct dyn_desc *own;
	int hit = desc_by_syspath(syspath, &own) != NULL;
	free(own);
	trc("enum_add_syspath e=%p syspath=%s hit=%d", (void *)e, syspath, hit);
	if (hit)
		list_append(&e->results, syspath, NULL);
	return 0;
}

static int enum_matches(struct udev_enumerate *e, const struct dev_desc *d) {
	/* nomatch subsystem wins */
	for (size_t i = 0; i < e->n_nomatch_subsystem; i++)
		if (strcmp(d->subsystem, e->nomatch_subsystem[i]) == 0)
			return 0;
	/* match subsystem (if any specified) */
	if (e->n_match_subsystem) {
		int ok = 0;
		for (size_t i = 0; i < e->n_match_subsystem; i++)
			if (strcmp(d->subsystem, e->match_subsystem[i]) == 0) { ok = 1; break; }
		if (!ok) return 0;
	}
	/* match sysname (if any specified). Real libudev matches sysname with
	 * fnmatch() shell-glob semantics, not a literal compare: smithay's
	 * gpus_for_seat() enumerates DRM nodes with match_sysname("card[0-9]*"),
	 * so a strcmp here filtered out card0 and left the udev-backend GPU list
	 * empty (this is what forced anvil's ANVIL_DRM_DEVICE direct-add patch).
	 * cosmic-comp has no such direct-add path — only post-enumeration
	 * ALLOW/BLOCK filters — so proper glob matching here is required for its
	 * kms backend to discover card0. A pattern with no glob metacharacters
	 * degrades to an exact match under fnmatch, so existing callers are
	 * unaffected. */
	if (e->n_match_sysname) {
		int ok = 0;
		for (size_t i = 0; i < e->n_match_sysname; i++)
			if (fnmatch(e->match_sysname[i], d->sysname, 0) == 0) { ok = 1; break; }
		if (!ok) return 0;
	}
	/* match property (if any specified) — all must match */
	for (size_t i = 0; i < e->n_match_prop; i++) {
		const char *want = e->match_prop_val[i];
		const char *have = NULL;
		for (size_t j = 0; j < d->nprops; j++)
			if (strcmp(d->props[j].key, e->match_prop_key[i]) == 0) { have = d->props[j].val; break; }
		if (!have) return 0;
		if (want && strcmp(have, want) != 0) return 0;
	}
	return 1;
}

int udev_enumerate_scan_devices(struct udev_enumerate *e) {
	if (!e) return -EINVAL;
	list_free(e->results);
	e->results = NULL;
	for (size_t i = 0; i < g_ndevices; i++) {
		const struct dev_desc *d = &g_devices[i];
		/* Only surface real device nodes (with a devnode) during a device
		 * scan; bare parent nodes are reachable via get_parent, not here. */
		if (!d->devnode) continue;
		if (enum_matches(e, d))
			list_append(&e->results, d->syspath, NULL);
	}
	/* Input nodes: a fresh coldplug scan of /dev/input/hotplug. */
	struct input_info snap[MAX_INPUTS];
	pthread_mutex_lock(&g_inputs_lock);
	inputs_rescan_locked();
	memcpy(snap, g_inputs, sizeof snap);
	pthread_mutex_unlock(&g_inputs_lock);
	for (int i = 0; i < MAX_INPUTS; i++) {
		if (!snap[i].present) continue;
		struct dyn_desc *x = dyn_input(i, 0, &snap[i]);
		if (!x) continue;
		if (enum_matches(e, &x->d))
			list_append(&e->results, x->syspath, NULL);
		free(x);
	}
	size_t n = 0;
	for (struct udev_list_entry *le = e->results; le; le = le->next) n++;
	trc("enum_scan e=%p rc=%d n=%zu", (void *)e, 0, n);
	for (struct udev_list_entry *le = e->results; le; le = le->next)
		trc("enum_entry e=%p syspath=%s", (void *)e, le->name);
	return 0;
}

int udev_enumerate_scan_subsystems(struct udev_enumerate *e) {
	if (!e) return -EINVAL;
	/* We do not model a subsystems tree; report an empty scan (success). */
	list_free(e->results);
	e->results = NULL;
	return 0;
}

struct udev_list_entry *udev_enumerate_get_list_entry(struct udev_enumerate *e) {
	struct udev_list_entry *head = e ? e->results : NULL;
	trc("enum_get_list e=%p head=%p", (void *)e, (void *)head);
	return head;
}

/* ===================================================================== */
/* udev_monitor — input hotplug from /dev/input/hotplug                   */
/* ===================================================================== */
/*
 * A monitor that may see the "input" subsystem (no subsystem filter, or an
 * "input" one) reads /dev/input/hotplug once enabled; its fd is that node's,
 * pollable, readable while a record is queued. Any other monitor (smithay's
 * drm one) keeps a socketpair end that never fires: nothing else hotplugs.
 */

#define MON_MAX_FILTERS 8

struct udev_monitor {
	int refcount;
	struct udev *udev;
	int fd;        /* returned by get_fd: socketpair end, then the hotplug node */
	int peer_fd;   /* socketpair peer, closed once the hotplug node replaces it */
	int enabled;
	int hotplug;   /* fd is /dev/input/hotplug */
	char filter_sub[MON_MAX_FILTERS][32];
	size_t n_filter;
};

static int monitor_wants(const struct udev_monitor *m, const char *subsystem) {
	if (m->n_filter == 0) return 1;
	for (size_t i = 0; i < m->n_filter; i++)
		if (strcmp(m->filter_sub[i], subsystem) == 0) return 1;
	return 0;
}

struct udev_monitor *udev_monitor_new_from_netlink(struct udev *udev, const char *name) {
	struct udev_monitor *m = calloc(1, sizeof(*m));
	if (!m) {
		errno = ENOMEM;
		trc("mon_new src=%s mon=(nil)", name ? name : "(null)");
		return NULL;
	}
	int sv[2];
	if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv) < 0) {
		int err = errno;
		free(m);
		errno = err;
		trc("mon_new src=%s mon=(nil)", name ? name : "(null)");
		return NULL;
	}
	m->refcount = 1;
	m->udev = udev_ref(udev);
	m->fd = sv[0];
	m->peer_fd = sv[1];
	trc("mon_new src=%s mon=%p", name ? name : "(null)", (void *)m);
	return m;
}

struct udev_monitor *udev_monitor_ref(struct udev_monitor *m) {
	if (m) m->refcount++;
	return m;
}

struct udev_monitor *udev_monitor_unref(struct udev_monitor *m) {
	if (!m) return NULL;
	if (--m->refcount > 0) return NULL;
	if (m->fd >= 0) close(m->fd);
	if (m->peer_fd >= 0) close(m->peer_fd);
	udev_unref(m->udev);
	free(m);
	return NULL;
}

struct udev *udev_monitor_get_udev(struct udev_monitor *m) { return m ? m->udev : NULL; }

int udev_monitor_enable_receiving(struct udev_monitor *m) {
	if (!m) {
		trc("mon_enable mon=(nil) rc=%d", -EINVAL);
		return -EINVAL;
	}
	if (!m->enabled && monitor_wants(m, "input")) {
		int hp = open(HOTPLUG_NODE, O_RDONLY | O_NONBLOCK | O_CLOEXEC);
		if (hp >= 0) {
			/* The coldplug records describe devices that exist already —
			 * the caller enumerates those itself; only later changes are
			 * events. */
			char line[96];
			for (int i = 0; i < 4 * MAX_INPUTS && read(hp, line, sizeof line) > 0; i++) {}
			/* Keep the fd number a caller may already hold from get_fd. */
			if (dup2(hp, m->fd) == m->fd) {
				fcntl(m->fd, F_SETFD, FD_CLOEXEC);
				close(hp);
			} else {
				close(m->fd);
				m->fd = hp;
			}
			close(m->peer_fd);
			m->peer_fd = -1;
			m->hotplug = 1;
		}
	}
	m->enabled = 1;
	trc("mon_enable mon=%p hotplug=%d rc=%d", (void *)m, m->hotplug, 0);
	return 0;
}

int udev_monitor_set_receive_buffer_size(struct udev_monitor *m, int size) {
	(void)size;
	return m ? 0 : -EINVAL;
}

int udev_monitor_get_fd(struct udev_monitor *m) {
	int fd = m ? m->fd : -EINVAL;
	trc("mon_fd mon=%p fd=%d", (void *)m, fd);
	return fd;
}

struct udev_device *udev_monitor_receive_device(struct udev_monitor *m) {
	if (!m || !m->hotplug) {
		errno = EAGAIN;
		trc("mon_recv mon=%p -> %p", (void *)m, (void *)NULL);
		return NULL;
	}
	char line[96];
	for (int tries = 0; tries < 4 * MAX_INPUTS; tries++) {
		ssize_t r = read(m->fd, line, sizeof line - 1);
		if (r <= 0) break;
		line[r] = 0;
		char act[8], kind[8]; int n; unsigned b, v, p;
		if (!parse_record(line, act, &n, kind, &b, &v, &p)) continue;
		int add = strcmp(act, "add") == 0;
		struct input_info in;
		pthread_mutex_lock(&g_inputs_lock);
		if (!g_inputs_scanned) inputs_rescan_locked();
		if (add) {
			probe_input(&g_inputs[n], n, kind, b, v, p);
			in = g_inputs[n];
		} else {
			in = g_inputs[n];
			if (!in.present) { /* never seen: describe it from the record */
				memset(&in, 0, sizeof in);
				in.cls = cls_from_kind(kind);
				in.bus = b; in.vendor = v; in.product = p;
			}
			memset(&g_inputs[n], 0, sizeof g_inputs[n]);
		}
		pthread_mutex_unlock(&g_inputs_lock);
		struct dyn_desc *x = dyn_input(n, 0, &in);
		struct udev_device *d = x ? device_wrap(m->udev, &x->d, x) : NULL;
		if (!d) continue;
		snprintf(d->action, sizeof d->action, "%s", add ? "add" : "remove");
		trc("mon_recv mon=%p action=%s syspath=%s -> %p", (void *)m, d->action, x->syspath, (void *)d);
		return d;
	}
	errno = EAGAIN;
	trc("mon_recv mon=%p -> %p", (void *)m, (void *)NULL);
	return NULL;
}

int udev_monitor_filter_add_match_subsystem_devtype(struct udev_monitor *m, const char *subsystem, const char *devtype) {
	trc("mon_filter mon=%p sub=%s devtype=%s",
	    (void *)m, subsystem ? subsystem : "(null)", devtype ? devtype : "(null)");
	if (!m) return -EINVAL;
	if (subsystem && m->n_filter < MON_MAX_FILTERS)
		snprintf(m->filter_sub[m->n_filter++], sizeof m->filter_sub[0], "%s", subsystem);
	return 0;
}
int udev_monitor_filter_add_match_tag(struct udev_monitor *m, const char *tag) {
	(void)tag;
	return m ? 0 : -EINVAL;
}
int udev_monitor_filter_update(struct udev_monitor *m) { return m ? 0 : -EINVAL; }
int udev_monitor_filter_remove(struct udev_monitor *m) { return m ? 0 : -EINVAL; }

/* ===================================================================== */
/* udev_queue — nothing is ever queued                                    */
/* ===================================================================== */

struct udev_queue {
	int refcount;
	struct udev *udev;
	int fd;
};

struct udev_queue *udev_queue_new(struct udev *udev) {
	struct udev_queue *q = calloc(1, sizeof(*q));
	if (!q) {
		errno = ENOMEM;
		return NULL;
	}
	q->refcount = 1;
	q->udev = udev_ref(udev);
	q->fd = -1;
	return q;
}

struct udev_queue *udev_queue_ref(struct udev_queue *q) {
	if (q) q->refcount++;
	return q;
}

struct udev_queue *udev_queue_unref(struct udev_queue *q) {
	if (!q) return NULL;
	if (--q->refcount > 0) return NULL;
	if (q->fd >= 0) close(q->fd);
	udev_unref(q->udev);
	free(q);
	return NULL;
}

struct udev *udev_queue_get_udev(struct udev_queue *q) { return q ? q->udev : NULL; }
int udev_queue_get_udev_is_active(struct udev_queue *q) { (void)q; return 0; /* no udevd */ }
int udev_queue_get_queue_is_empty(struct udev_queue *q) { (void)q; return 1; /* always empty */ }
int udev_queue_flush(struct udev_queue *q) { return q ? 0 : -EINVAL; }

int udev_queue_get_fd(struct udev_queue *q) {
	if (!q) return -EINVAL;
	if (q->fd < 0) {
		/* an fd that never signals "queue changed" */
		int sv[2];
		if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv) < 0)
			return -errno;
		q->fd = sv[0];
		close(sv[1]);
	}
	return q->fd;
}

/* deprecated queue getters */
unsigned long long int udev_queue_get_kernel_seqnum(struct udev_queue *q) { (void)q; return 0; }
unsigned long long int udev_queue_get_udev_seqnum(struct udev_queue *q) { (void)q; return 0; }
int udev_queue_get_seqnum_is_finished(struct udev_queue *q, unsigned long long int seqnum) {
	(void)q; (void)seqnum; return 1;
}
int udev_queue_get_seqnum_sequence_is_finished(struct udev_queue *q, unsigned long long int start, unsigned long long int end) {
	(void)q; (void)start; (void)end; return 1;
}
struct udev_list_entry *udev_queue_get_queued_list_entry(struct udev_queue *q) { (void)q; return NULL; }

/* ===================================================================== */
/* udev_hwdb — empty static database                                      */
/* ===================================================================== */

struct udev_hwdb {
	int refcount;
	struct udev *udev;
};

struct udev_hwdb *udev_hwdb_new(struct udev *udev) {
	struct udev_hwdb *h = calloc(1, sizeof(*h));
	if (!h) {
		errno = ENOMEM;
		return NULL;
	}
	h->refcount = 1;
	h->udev = udev_ref(udev);
	return h;
}

struct udev_hwdb *udev_hwdb_ref(struct udev_hwdb *h) {
	if (h) h->refcount++;
	return h;
}

struct udev_hwdb *udev_hwdb_unref(struct udev_hwdb *h) {
	if (!h) return NULL;
	if (--h->refcount > 0) return NULL;
	udev_unref(h->udev);
	free(h);
	return NULL;
}

struct udev_list_entry *udev_hwdb_get_properties_list_entry(struct udev_hwdb *h, const char *modalias, unsigned flags) {
	(void)h;
	(void)modalias;
	(void)flags;
	return NULL; /* no hwdb entries */
}

/* ===================================================================== */
/* udev_util                                                              */
/* ===================================================================== */

/*
 * Faithful port of udev's whitelist encoder: keeps [0-9A-Za-z#+-.:=@_/] as-is
 * and replaces every other byte with "\xNN". Returns 0 on success, <0 if the
 * output buffer is too small (partial output like systemd).
 */
static int allowed_char(unsigned char c) {
	if ((c >= '0' && c <= '9') || (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z'))
		return 1;
	return strchr("#+-.:=@_/", c) != NULL;
}

int udev_util_encode_string(const char *str, char *str_enc, size_t len) {
	if (!str || !str_enc || len == 0)
		return -EINVAL;
	size_t j = 0;
	for (size_t i = 0; str[i] != '\0'; i++) {
		unsigned char c = (unsigned char)str[i];
		if (allowed_char(c)) {
			if (j + 1 >= len) goto nospace;
			str_enc[j++] = (char)c;
		} else {
			if (j + 4 >= len) goto nospace;
			int n = snprintf(&str_enc[j], len - j, "\\x%02x", c);
			if (n != 4) goto nospace;
			j += 4;
		}
	}
	str_enc[j] = '\0';
	return 0;
nospace:
	str_enc[j] = '\0';
	return -ENOMEM;
}
