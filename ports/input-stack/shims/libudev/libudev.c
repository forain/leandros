/*
 * libudev.c — LeandrOS libudev ABI shim (soname libudev.so.1).
 *
 * Implements the subset of libudev that libinput, smithay's `udev` crate, and
 * Mesa actually import, plus safe stubs for the rest of the public ABI so that
 * *any* consumer links. There is no udevd and no netlink uevent source on
 * LeandrOS; instead this shim serves a small, DATA-DRIVEN static device model
 * describing the fixed QEMU device set:
 *
 *     /dev/dri/card0        subsystem=drm    (GPU scanout node)
 *     /dev/dri/renderD128   subsystem=drm    (render node)
 *     /dev/input/event0     subsystem=input  ID_INPUT_KEYBOARD
 *     /dev/input/event1     subsystem=input  ID_INPUT_MOUSE   (virtio-tablet:
 *                                              absolute ABS 0..32767 + BTN_LEFT,
 *                                              no INPUT_PROP_DIRECT -> a pointer,
 *                                              not a touchscreen; see evtest2)
 *
 * M4 note: the kernel's fixed device set is event0=keyboard, event1=virtio-
 * tablet only (no third input node). An earlier iteration of this table also
 * modeled a synthetic event2 touchscreen; that entry has been removed since
 * it no longer corresponds to any device the kernel actually exposes -- a
 * libinput udev enumeration that included it would try (and fail) to open a
 * nonexistent /dev/input/event2.
 *
 * Enumeration returns the entries whose subsystem/sysname/property filters
 * match; devices are looked up by syspath / devnum / (subsystem,sysname).
 * The hotplug monitor returns a real, pollable fd (one end of a socketpair)
 * that never delivers an event — QEMU's device set is fixed at boot.
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
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/types.h>

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
static const struct prop props_kbd[] = {
	{ "DEVNAME", "/dev/input/event0" },
	{ "ID_INPUT", "1" },
	{ "ID_INPUT_KEYBOARD", "1" },
	{ "ID_SEAT", "seat0" },
};
static const struct prop props_mouse[] = {
	{ "DEVNAME", "/dev/input/event1" },
	{ "ID_INPUT", "1" },
	{ "ID_INPUT_MOUSE", "1" },
	{ "ID_SEAT", "seat0" },
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

	/* --- input: parent input<N> nodes (no devnode) --- */
	{ "/sys/class/input/input0", "input0", "0", "input", NULL, NULL, NULL,
	  0, 0, 0, 1, NULL, NULL, 0 },
	{ "/sys/class/input/input1", "input1", "1", "input", NULL, NULL, NULL,
	  0, 0, 0, 1, NULL, NULL, 0 },

	/* --- input: evdev nodes --- */
	{ "/sys/class/input/event0", "event0", "0", "input", NULL, "/dev/input/event0", NULL,
	  'c', 13, 64, 1, "/sys/class/input/input0", props_kbd, NPROPS(props_kbd) },
	{ "/sys/class/input/event1", "event1", "1", "input", NULL, "/dev/input/event1", NULL,
	  'c', 13, 65, 1, "/sys/class/input/input1", props_mouse, NPROPS(props_mouse) },
};
static const size_t g_ndevices = sizeof(g_devices) / sizeof(g_devices[0]);

static const struct dev_desc *desc_by_syspath(const char *syspath) {
	if (!syspath) return NULL;
	for (size_t i = 0; i < g_ndevices; i++)
		if (strcmp(g_devices[i].syspath, syspath) == 0)
			return &g_devices[i];
	return NULL;
}

static const struct dev_desc *desc_by_devnum(char kind, dev_t num) {
	unsigned ma = major(num), mi = minor(num);
	for (size_t i = 0; i < g_ndevices; i++) {
		const struct dev_desc *d = &g_devices[i];
		if (d->dev_kind == kind && d->major == ma && d->minor == mi)
			return d;
	}
	return NULL;
}

static const struct dev_desc *desc_by_subsystem_sysname(const char *subsystem, const char *sysname) {
	if (!subsystem || !sysname) return NULL;
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
};

static struct udev_device *device_wrap(struct udev *udev, const struct dev_desc *desc) {
	if (!desc) return NULL;
	struct udev_device *d = calloc(1, sizeof(*d));
	if (!d) {
		errno = ENOMEM;
		return NULL;
	}
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
	free(d);
	return NULL;
}

struct udev *udev_device_get_udev(struct udev_device *d) {
	return d ? d->udev : NULL;
}

struct udev_device *udev_device_new_from_syspath(struct udev *udev, const char *syspath) {
	const struct dev_desc *desc = desc_by_syspath(syspath);
	if (!desc) {
		errno = ENODEV;
		return NULL;
	}
	return device_wrap(udev, desc);
}

struct udev_device *udev_device_new_from_devnum(struct udev *udev, char type, dev_t devnum) {
	const struct dev_desc *desc = desc_by_devnum(type, devnum);
	if (!desc) {
		errno = ENODEV;
		return NULL;
	}
	return device_wrap(udev, desc);
}

struct udev_device *udev_device_new_from_subsystem_sysname(struct udev *udev, const char *subsystem, const char *sysname) {
	const struct dev_desc *desc = desc_by_subsystem_sysname(subsystem, sysname);
	if (!desc) {
		errno = ENODEV;
		return NULL;
	}
	return device_wrap(udev, desc);
}

struct udev_device *udev_device_new_from_device_id(struct udev *udev, const char *id) {
	/* id form: "c226:0" / "b8:0" / "+subsystem:sysname". Support the
	 * char/block numeric form used in practice; others -> ENODEV. */
	if (!id || (id[0] != 'c' && id[0] != 'b')) {
		errno = EINVAL;
		return NULL;
	}
	char kind = id[0];
	unsigned ma = 0, mi = 0;
	if (sscanf(id + 1, "%u:%u", &ma, &mi) != 2) {
		errno = EINVAL;
		return NULL;
	}
	return udev_device_new_from_devnum(udev, kind, makedev(ma, mi));
}

struct udev_device *udev_device_new_from_environment(struct udev *udev) {
	(void)udev;
	errno = ENODEV; /* no uevent environment on LeandrOS */
	return NULL;
}

struct udev_device *udev_device_get_parent(struct udev_device *d) {
	if (!d) return NULL;
	if (d->parent) return d->parent;
	if (!d->desc->parent_syspath) return NULL;
	const struct dev_desc *pd = desc_by_syspath(d->desc->parent_syspath);
	if (!pd) return NULL;
	d->parent = device_wrap(d->udev, pd);
	return d->parent; /* owned by child, not an extra ref for the caller */
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
		if (sub_ok && dt_ok)
			return p;
		p = udev_device_get_parent(p);
	}
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
const char *udev_device_get_devnode(struct udev_device *d) { return d ? d->desc->devnode : NULL; }
const char *udev_device_get_driver(struct udev_device *d) { return d ? d->desc->driver : NULL; }
const char *udev_device_get_action(struct udev_device *d) { (void)d; return NULL; /* not from a uevent */ }

int udev_device_get_is_initialized(struct udev_device *d) { return d ? d->desc->is_initialized : 0; }

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
	if (!d) return NULL;
	device_build_properties(d);
	return d->properties;
}

const char *udev_device_get_property_value(struct udev_device *d, const char *key) {
	if (!d || !key) return NULL;
	for (size_t i = 0; i < d->desc->nprops; i++)
		if (strcmp(d->desc->props[i].key, key) == 0)
			return d->desc->props[i].val;
	return NULL;
}

struct udev_list_entry *udev_device_get_devlinks_list_entry(struct udev_device *d) {
	(void)d;
	return NULL; /* no /dev/input/by-id or by-path symlinks modeled */
}

struct udev_list_entry *udev_device_get_tags_list_entry(struct udev_device *d) { (void)d; return NULL; }
struct udev_list_entry *udev_device_get_current_tags_list_entry(struct udev_device *d) { (void)d; return NULL; }
struct udev_list_entry *udev_device_get_sysattr_list_entry(struct udev_device *d) { (void)d; return NULL; }

const char *udev_device_get_sysattr_value(struct udev_device *d, const char *sysattr) {
	(void)d;
	(void)sysattr;
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
	return e;
}

struct udev_enumerate *udev_enumerate_ref(struct udev_enumerate *e) {
	if (e) e->refcount++;
	return e;
}

struct udev_enumerate *udev_enumerate_unref(struct udev_enumerate *e) {
	if (!e) return NULL;
	if (--e->refcount > 0) return NULL;
	list_free(e->results);
	udev_unref(e->udev);
	free(e);
	return NULL;
}

struct udev *udev_enumerate_get_udev(struct udev_enumerate *e) { return e ? e->udev : NULL; }

int udev_enumerate_add_match_subsystem(struct udev_enumerate *e, const char *subsystem) {
	if (!e || !subsystem) return -EINVAL;
	if (e->n_match_subsystem < MAX_FILTERS)
		e->match_subsystem[e->n_match_subsystem++] = subsystem;
	return 0;
}
int udev_enumerate_add_nomatch_subsystem(struct udev_enumerate *e, const char *subsystem) {
	if (!e || !subsystem) return -EINVAL;
	if (e->n_nomatch_subsystem < MAX_FILTERS)
		e->nomatch_subsystem[e->n_nomatch_subsystem++] = subsystem;
	return 0;
}
int udev_enumerate_add_match_sysname(struct udev_enumerate *e, const char *sysname) {
	if (!e || !sysname) return -EINVAL;
	if (e->n_match_sysname < MAX_FILTERS)
		e->match_sysname[e->n_match_sysname++] = sysname;
	return 0;
}
int udev_enumerate_add_match_property(struct udev_enumerate *e, const char *property, const char *value) {
	if (!e || !property) return -EINVAL;
	if (e->n_match_prop < MAX_FILTERS) {
		e->match_prop_key[e->n_match_prop] = property;
		e->match_prop_val[e->n_match_prop] = value;
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
	(void)e; (void)tag; return 0;
}
int udev_enumerate_add_match_parent(struct udev_enumerate *e, struct udev_device *parent) {
	(void)e; (void)parent; return 0;
}
int udev_enumerate_add_match_is_initialized(struct udev_enumerate *e) {
	(void)e; return 0; /* everything is initialized */
}
int udev_enumerate_add_syspath(struct udev_enumerate *e, const char *syspath) {
	if (!e || !syspath) return -EINVAL;
	if (desc_by_syspath(syspath))
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
	/* match sysname (if any specified) */
	if (e->n_match_sysname) {
		int ok = 0;
		for (size_t i = 0; i < e->n_match_sysname; i++)
			if (strcmp(d->sysname, e->match_sysname[i]) == 0) { ok = 1; break; }
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
	return e ? e->results : NULL;
}

/* ===================================================================== */
/* udev_monitor — pollable fd that never fires                            */
/* ===================================================================== */

struct udev_monitor {
	int refcount;
	struct udev *udev;
	int fd;        /* our end of a socketpair; readable end returned to caller */
	int peer_fd;   /* never written -> monitor never delivers */
	int enabled;
};

struct udev_monitor *udev_monitor_new_from_netlink(struct udev *udev, const char *name) {
	(void)name;
	struct udev_monitor *m = calloc(1, sizeof(*m));
	if (!m) {
		errno = ENOMEM;
		return NULL;
	}
	int sv[2];
	if (socketpair(AF_UNIX, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0, sv) < 0) {
		int err = errno;
		free(m);
		errno = err;
		return NULL;
	}
	m->refcount = 1;
	m->udev = udev_ref(udev);
	m->fd = sv[0];
	m->peer_fd = sv[1];
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
	if (!m) return -EINVAL;
	m->enabled = 1;
	return 0;
}

int udev_monitor_set_receive_buffer_size(struct udev_monitor *m, int size) {
	(void)size;
	return m ? 0 : -EINVAL;
}

int udev_monitor_get_fd(struct udev_monitor *m) {
	return m ? m->fd : -EINVAL;
}

struct udev_device *udev_monitor_receive_device(struct udev_monitor *m) {
	/* The fd never becomes readable, but a defensive caller may still poll
	 * spuriously and call here — report "no event pending". */
	(void)m;
	errno = EAGAIN;
	return NULL;
}

int udev_monitor_filter_add_match_subsystem_devtype(struct udev_monitor *m, const char *subsystem, const char *devtype) {
	(void)subsystem;
	(void)devtype;
	return m ? 0 : -EINVAL;
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
