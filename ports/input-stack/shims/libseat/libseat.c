/*
 * libseat.c — LeandrOS libseat ABI shim ("builtin, always-root" backend).
 *
 * Implements the full libseat.h ABI (soname libseat.so.1) for a system that
 * runs the compositor as root with a single fixed seat and no VT switching.
 * This lets smithay / wlroots use libseat unmodified while the OS provides no
 * seatd, no logind, and no VT layer.
 *
 * Model
 * -----
 *   - Exactly one seat, named "seat0".
 *   - The process is assumed to already have permission to open DRM and evdev
 *     nodes (it runs as root), so open_device() is a plain open() — no
 *     privileged helper, no drmSetMaster brokering (the compositor calls
 *     DRM SET_MASTER itself once it holds the fd).
 *   - No session switching: switch_session() is a no-op that reports success.
 *   - enable_seat is delivered exactly once, synchronously, from
 *     libseat_open_seat() — matching the plan's D3 contract and the fact that
 *     real libseat may also call back synchronously during open. Callers
 *     (smithay's libseat-rs) install their userdata before the open call
 *     precisely to tolerate this. (On a kernel with VT support and a VT that
 *     is not foreground at open time, this is skipped — see below.)
 *   - get_fd() returns a real, pollable eventfd that is never signalled, so a
 *     caller can register it in its event loop and it simply never wakes.
 *     dispatch() therefore always reports "0 messages processed".
 *
 * VT awareness (TODO.md item 14, piece 3)
 * ----------------------------------------
 * The paragraph above is the *fallback* behaviour and is exactly what a
 * kernel with no VT support does. smithay's KMS paths read is_active() and
 * early-return when inactive, so wherever real VT switching exists,
 * enable_seat/disable_seat must actually fire on real activate/deactivate
 * transitions or the compositor will never resume KMS output after a switch
 * back.
 *
 * The kernel now exposes standard Linux VT semantics: /dev/tty0 ("the active
 * VT") plus numbered /dev/tty1.../dev/tty6, VT_GETSTATE (struct vt_stat)
 * for state, VT_ACTIVATE/VT_WAITACTIVE for switching. This shim uses exactly
 * one corner of that: at libseat_open_seat() time it opens /dev/tty0 and
 * confirms VT_GETSTATE answers. On a kernel that predates this (or any build
 * where /dev/tty0 does not exist, or the ioctl comes back ENOTTY), that
 * probe fails cleanly and the shim behaves exactly as the fallback
 * paragraph above describes -- single always-active seat, disable_seat
 * never emitted. See vt_probe() for the exact rule.
 *
 * When the probe succeeds:
 *   - get_fd() returns the /dev/tty0 fd instead of the inert eventfd; the
 *     kernel makes it poll-readable when the active VT has changed since
 *     this open last read it, and read() yields one byte holding the new
 *     active VT number (edge state is per-open, so this doesn't race other
 *     openers).
 *   - dispatch() drains that byte and then re-queries authoritative state
 *     via VT_GETSTATE rather than trusting the byte -- see the comment on
 *     libseat_dispatch() for why -- and fires enable_seat/disable_seat
 *     exactly on transitions of *this seat's* VT between foreground and
 *     not.
 *   - "this seat's VT" is not the active VT; it is the VT the compositor
 *     itself owns, which the shim has to determine independently of any
 *     notification -- see owned_vt() for how and why.
 *
 * Consumer import set satisfied (derived from libseat-sys 0.2.0 extern "C"
 * block + libseat.h): libseat_open_seat, _close_seat, _disable_seat,
 * _open_device, _close_device, _seat_name, _switch_session, _get_fd,
 * _dispatch, _set_log_level, _set_log_handler.
 */

#include "libseat.h"

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <sys/eventfd.h>
#include <sys/ioctl.h>

#include <linux/vt.h> /* VT_GETSTATE, struct vt_stat, MAX_NR_CONSOLES */

#define SEAT_NAME "seat0"

/* The kernel's "active VT" alias node -- always names whichever VT is
 * currently in the foreground, regardless of which VT this process owns. */
#define VT0_PATH "/dev/tty0"

struct libseat {
	const struct libseat_seat_listener *listener;
	void *userdata;
	int conn_fd;   /* eventfd; pollable, never signalled (no-VT fallback) */
	int vt_fd;     /* VT0_PATH fd if the kernel supports it, else -1 */
	int own_vtnr;  /* 1-based VT this seat owns, or -1 if unknown */
	int active;
};

/* ===================================================================== */
/* Self-tracing (LEANDROS_INPUT_TRACE)                                    */
/*                                                                         */
/* Mirrors the libudev shim's tracer: "[SEATSHIM] pid=<pid> ..." lines so */
/* stderr interleaved from multiple processes (cosmic-comp, cosmic-panel, */
/* cosmic-settings-daemon, Mesa) stays attributable. Disabled unless      */
/* LEANDROS_INPUT_TRACE is set to something other than "" or "0"; the     */
/* getenv() result is cached so it is read exactly once. Hard-capped at   */
/* SEAT_TRACE_BUDGET lines total. trc() saves/restores errno around its   */
/* own work so tracing is provably invisible to errno-reading callers.   */
/* Mirrored to LEANDROS_INPUT_TRACE_DIR/seatshim.<pid>.log (if set) since */
/* stderr alone is not reliable: cosmic-session may repoint fd 2 out from */
/* under a child it launches; see trace_file() below.                    */
/* ===================================================================== */

#define SEAT_TRACE_BUDGET 200

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
 * path, gets a per-process file "<dir>/seatshim.<pid>.log" in addition to
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
	int n = snprintf(path, sizeof(path), "%s/seatshim.%d.log", dir, (int)getpid());
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
	if (n_lines >= SEAT_TRACE_BUDGET) {
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
	snprintf(line, sizeof(line), "[SEATSHIM] pid=%d %s\n", (int)getpid(), msg);

	fputs(line, stderr);
	fflush(stderr);
	FILE *tf = trace_file();
	if (tf != NULL) {
		fputs(line, tf);
		fflush(tf);
	}

	if (n_lines == SEAT_TRACE_BUDGET) {
		char exhausted[128];
		snprintf(exhausted, sizeof(exhausted),
		         "[SEATSHIM] pid=%d TRACE BUDGET EXHAUSTED after %d lines\n",
		         (int)getpid(), SEAT_TRACE_BUDGET);
		fputs(exhausted, stderr);
		fflush(stderr);
		if (tf != NULL) {
			fputs(exhausted, tf);
			fflush(tf);
		}
	}

	errno = saved_errno;
}

/* -------- logging (accepted, effectively silent by default) -------- */

static libseat_log_func g_log_handler = NULL;
static enum libseat_log_level g_log_level = LIBSEAT_LOG_LEVEL_SILENT;

void libseat_set_log_handler(libseat_log_func handler) {
	g_log_handler = handler;
}

void libseat_set_log_level(enum libseat_log_level level) {
	g_log_level = level;
}

/* -------- VT probing -------- */

/* XDG_VTNR: the conventional env var login stacks (pam_systemd, and any
 * future greetd VT-mode session launch) set to tell a session which VT it
 * was placed on. Nothing in this tree sets it yet, but honouring it costs
 * nothing and matches the ecosystem convention smithay/wlroots consumers
 * already expect -- if it's ever set, prefer it outright. Returns a 1-based
 * VT number, or -1 if unset/unparseable. */
static int owned_vt_from_env(void) {
	const char *env = getenv("XDG_VTNR");
	if (env == NULL || env[0] == '\0') {
		return -1;
	}
	char *end = NULL;
	long v = strtol(env, &end, 10);
	if (end == env || *end != '\0' || v <= 0 || v > MAX_NR_CONSOLES) {
		trc("owned_vt_from_env: XDG_VTNR=%s unparseable/out of range", env);
		return -1;
	}
	return (int)v;
}

/* Fallback: derive the owning VT from the process's controlling terminal.
 * This is what wlroots' non-logind "direct" session backend does when
 * XDG_VTNR isn't set, and it needs no cooperation from whatever launched
 * this process -- a getty/login session that ran TIOCSCTTY against
 * /dev/ttyN leaves that exact path resolvable via open("/dev/tty") +
 * ttyname_r() regardless of what ended up on fd 0/1/2. Deliberately
 * conservative: anything that doesn't parse as "/dev/tty" followed by
 * digits (ttyUSB0, ttyS0, pts/3, ...) is rejected rather than guessed at.
 * /dev/tty0 itself parses as VT number 0, which the v <= 0 check below
 * rejects explicitly -- tty0 is the "active VT" alias, never a real VT, and
 * is never a process's controlling terminal in practice, but excluding it
 * on principle costs nothing. */
static int owned_vt_from_ctty(void) {
	int fd = open("/dev/tty", O_RDONLY | O_NOCTTY | O_CLOEXEC);
	if (fd < 0) {
		trc("owned_vt_from_ctty: open(/dev/tty) failed errno=%d (no controlling terminal?)", errno);
		return -1;
	}
	char buf[64];
	int rc = ttyname_r(fd, buf, sizeof(buf));
	close(fd);
	if (rc != 0) {
		trc("owned_vt_from_ctty: ttyname_r failed rc=%d", rc);
		return -1;
	}

	static const char prefix[] = "/dev/tty";
	size_t plen = sizeof(prefix) - 1;
	if (strncmp(buf, prefix, plen) != 0) {
		trc("owned_vt_from_ctty: ctty=%s not under %s", buf, prefix);
		return -1;
	}
	const char *digits = buf + plen;
	if (digits[0] < '0' || digits[0] > '9') {
		/* ttyUSB0, ttyS0, ttyAMA0, ... -- a real tty, just not a VT. */
		trc("owned_vt_from_ctty: ctty=%s is not a numbered VT", buf);
		return -1;
	}
	char *end = NULL;
	long v = strtol(digits, &end, 10);
	if (end == digits || *end != '\0' || v <= 0 || v > MAX_NR_CONSOLES) {
		/* v == 0 rejects /dev/tty0 itself, the "active VT" alias, which is
		 * never a process's controlling terminal in practice but is
		 * rejected on principle rather than by accident. */
		trc("owned_vt_from_ctty: ctty=%s out of VT range", buf);
		return -1;
	}
	return (int)v;
}

/* Which VT does *this* seat own? Upstream seatd never has to answer this:
 * a real seatd/logind is the process that launched the session onto a VT
 * in the first place, so it always already knows. We have no such daemon,
 * so the compositor process has to work it out for itself, the same way
 * wlroots' direct (non-logind) session backend does: prefer XDG_VTNR,
 * fall back to the controlling terminal. If neither resolves, ownership is
 * unknowable -- and guessing would be actively harmful (see the header
 * comment on vt_probe()), so the caller must treat that the same as "no VT
 * support" rather than assume VT 1 or similar. */
static int owned_vt(void) {
	int v = owned_vt_from_env();
	if (v > 0) {
		trc("owned_vt: %d (from XDG_VTNR)", v);
		return v;
	}
	v = owned_vt_from_ctty();
	if (v > 0) {
		trc("owned_vt: %d (from controlling terminal)", v);
		return v;
	}
	trc("owned_vt: unknown (no XDG_VTNR, no VT controlling terminal)");
	return -1;
}

/* Probe for kernel VT support and, if present, this seat's initial active
 * state. On success: *out_active reflects whether own_vtnr is currently the
 * foreground VT, *out_own_vtnr is that VT number, and the returned fd is
 * VT0_PATH, open and the caller's to keep (poll-readable when the kernel
 * later reports a transition). On any failure -- VT ownership unknowable,
 * no such device (ENOENT, ENXIO, ...), or a device that exists but doesn't
 * understand VT_GETSTATE (ENOTTY, EINVAL, ...) -- returns -1 and leaves the
 * out-params untouched; the caller falls back to always-active. This is the
 * only place that treats "no VT support" as ordinary, not exceptional:
 * everywhere else in the file simply branches on vt_fd < 0.
 *
 * Ownership is resolved *before* touching VT0_PATH at all: without it,
 * there is nothing to compare an active-VT number against, so opening the
 * device would be pure overhead on the way to the same fallback. */
static int vt_probe(unsigned int *out_active, int *out_own_vtnr) {
	int vtnr = owned_vt();
	if (vtnr < 0) {
		trc("vt_probe: VT ownership unknown -- no VT support, always-active fallback");
		return -1;
	}

	int fd = open(VT0_PATH, O_RDWR | O_CLOEXEC | O_NONBLOCK);
	if (fd < 0) {
		trc("vt_probe open(%s) failed errno=%d -- no VT support, always-active fallback", VT0_PATH,
		    errno);
		return -1;
	}

	struct vt_stat st;
	memset(&st, 0, sizeof(st));
	if (ioctl(fd, VT_GETSTATE, &st) != 0) {
		trc("vt_probe ioctl(%s, VT_GETSTATE) failed errno=%d -- no VT support, always-active "
		    "fallback",
		    VT0_PATH, errno);
		close(fd);
		return -1;
	}

	unsigned int active = (st.v_active == (unsigned short)vtnr) ? 1u : 0u;
	trc("vt_probe -> vt_fd=%d own_vtnr=%d v_active=%u active=%u (kernel VT support detected)", fd,
	    vtnr, (unsigned)st.v_active, active);
	*out_active = active;
	*out_own_vtnr = vtnr;
	return fd;
}

/* -------- seat lifecycle -------- */

struct libseat *libseat_open_seat(const struct libseat_seat_listener *listener, void *userdata) {
	if (listener == NULL || listener->enable_seat == NULL || listener->disable_seat == NULL) {
		errno = EINVAL;
		trc("open_seat listener=%p -> seat=(nil) fd=-1", (const void *)listener);
		return NULL;
	}

	struct libseat *seat = calloc(1, sizeof(*seat));
	if (seat == NULL) {
		errno = ENOMEM;
		trc("open_seat listener=%p -> seat=(nil) fd=-1", (const void *)listener);
		return NULL;
	}

	seat->listener = listener;
	seat->userdata = userdata;
	seat->active = 1;
	seat->vt_fd = -1;
	seat->own_vtnr = -1;

	/* A pollable fd that never becomes readable: the caller can add it to
	 * its event loop; it simply never fires (QEMU device set is fixed).
	 * Always opened, even when VT support is present, so behaviour is
	 * unchanged for callers that inspect get_fd() before the VT probe path
	 * exists anywhere else. */
	seat->conn_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
	if (seat->conn_fd < 0) {
		int e = errno;
		free(seat);
		errno = e;
		trc("open_seat listener=%p -> seat=(nil) fd=-1", (const void *)listener);
		return NULL;
	}

	/* Probe for kernel VT support and this seat's VT ownership. Fails
	 * cleanly (see vt_probe()) on a kernel without VT support, or when VT
	 * ownership can't be determined; either way seat->vt_fd stays -1 and
	 * everything below behaves exactly as it did before this probe
	 * existed. */
	unsigned int vt_active = 1;
	seat->vt_fd = vt_probe(&vt_active, &seat->own_vtnr);
	if (seat->vt_fd >= 0) {
		seat->active = vt_active ? 1 : 0;
	}

	/* Deliver activation immediately, per the D3 contract -- but only if
	 * the seat is actually active. Without VT support seat->active is
	 * always 1 here, so this is unconditional exactly as before. With VT
	 * support and a VT that isn't foreground at open time, enable_seat is
	 * correctly withheld until dispatch() observes an activate transition. */
	if (seat->active) {
		seat->listener->enable_seat(seat, seat->userdata);
	} else {
		trc("open_seat seat=%p starting inactive (VT %d not foreground); enable_seat deferred",
		    (void *)seat, seat->own_vtnr);
	}
	trc("open_seat listener=%p -> seat=%p fd=%d vt_fd=%d own_vtnr=%d active=%d",
	    (const void *)listener, (void *)seat, seat->conn_fd, seat->vt_fd, seat->own_vtnr,
	    seat->active);
	return seat;
}

int libseat_close_seat(struct libseat *seat) {
	trc("close_seat seat=%p", (void *)seat);
	if (seat == NULL) {
		errno = EINVAL;
		return -1;
	}
	if (seat->conn_fd >= 0) {
		close(seat->conn_fd);
	}
	if (seat->vt_fd >= 0) {
		close(seat->vt_fd);
	}
	free(seat);
	return 0;
}

int libseat_disable_seat(struct libseat *seat) {
	/* Client-side acknowledgement that devices have been released in
	 * response to a disable_seat() callback. Without VT support we never
	 * emit that callback, so this stays a no-op that reports success (for
	 * callers that defensively call it anyway). With VT support this still
	 * does not need to poke the kernel: the VT0_PATH notification contract
	 * (see the header comment) is fire-and-forget -- the kernel flips
	 * active state unilaterally and notifies -- not a blocking switch
	 * handshake like classic VT_PROCESS mode's VT_RELDISP. If the kernel
	 * side ever grows a real VT_SETMODE(VT_PROCESS)-style handshake, the
	 * VT_RELDISP ack belongs here, gated the same way vt_probe() gates
	 * everything else. */
	trc("disable_seat seat=%p", (void *)seat);
	if (seat == NULL) {
		errno = EINVAL;
		return -1;
	}
	seat->active = 0;
	return 0;
}

const char *libseat_seat_name(struct libseat *seat) {
	if (seat == NULL) {
		errno = EINVAL;
		trc("seat_name seat=(nil) -> (null)");
		return NULL;
	}
	trc("seat_name seat=%p -> %s", (void *)seat, SEAT_NAME);
	return SEAT_NAME;
}

/* -------- device open/close -------- */

int libseat_open_device(struct libseat *seat, const char *path, int *fd) {
	if (seat == NULL || path == NULL || fd == NULL) {
		errno = EINVAL;
		trc("open_device seat=%p path=%s -> id=-1 fd=-1 errno=%d",
		    (void *)seat, path ? path : "(null)", EINVAL);
		return -1;
	}
	/* Running as root with direct access: just open the node. The returned
	 * device id is the fd itself (close_device takes the id back). */
	int f = open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
	if (f < 0) {
		int saved_errno = errno; /* open() set errno; trace must not clobber it */
		trc("open_device seat=%p path=%s -> id=-1 fd=-1 errno=%d", (void *)seat, path, saved_errno);
		errno = saved_errno;
		return -1; /* open() set errno */
	}
	*fd = f;
	trc("open_device seat=%p path=%s -> id=%d fd=%d errno=0", (void *)seat, path, f, f);
	return f; /* device id == fd */
}

int libseat_close_device(struct libseat *seat, int device_id) {
	if (seat == NULL || device_id < 0) {
		errno = EINVAL;
		trc("close_device seat=%p id=%d rc=-1", (void *)seat, device_id);
		return -1;
	}
	if (close(device_id) < 0) {
		int saved_errno = errno; /* close() set errno; trace must not clobber it */
		trc("close_device seat=%p id=%d rc=-1", (void *)seat, device_id);
		errno = saved_errno;
		return -1; /* close() set errno */
	}
	trc("close_device seat=%p id=%d rc=0", (void *)seat, device_id);
	return 0;
}

/* -------- session switching (unsupported: single session) -------- */

int libseat_switch_session(struct libseat *seat, int session) {
	/* No VT / multi-session support. A switch request is silently accepted
	 * and has no effect, exactly as the API permits ("does not imply that a
	 * switch will occur"). */
	trc("switch_session seat=%p session=%d", (void *)seat, session);
	if (seat == NULL) {
		errno = EINVAL;
		return -1;
	}
	return 0;
}

/* -------- event dispatch -------- */

int libseat_get_fd(struct libseat *seat) {
	if (seat == NULL) {
		errno = EINVAL;
		trc("get_fd seat=(nil) -> -1");
		return -1;
	}
	/* Without VT support (vt_fd == -1, every kernel in this tree today):
	 * conn_fd, exactly as before -- pollable, never signalled. With VT
	 * support: vt_fd, which the kernel is expected to make readable on a
	 * foreground-VT transition. A caller only ever adds one fd to its event
	 * loop (that is the libseat contract), so whichever fd can actually
	 * carry a wakeup is the one that must be returned. */
	int fd = seat->vt_fd >= 0 ? seat->vt_fd : seat->conn_fd;
	trc("get_fd seat=%p -> %d (vt_fd=%d conn_fd=%d)", (void *)seat, fd, seat->vt_fd, seat->conn_fd);
	return fd;
}

int libseat_dispatch(struct libseat *seat, int timeout) {
	/* This is typically called once per event-loop iteration, so it is the
	 * dominant consumer of the trace budget; the shared budget/exhaustion
	 * line (SEAT_TRACE_BUDGET, in trc()) covers it like every other call. */
	if (seat == NULL) {
		errno = EINVAL;
		trc("dispatch seat=(nil) timeout=%d", timeout);
		return -1;
	}
	trc("dispatch seat=%p timeout=%d", (void *)seat, timeout);

	if (seat->vt_fd < 0) {
		/* No backend messages ever arrive (no seatd/logind connection), so
		 * there is nothing to process: report "0 messages processed"
		 * (success) without touching conn_fd. Nothing ever writes the
		 * eventfd counter, so a read(conn_fd) here would just return EAGAIN
		 * on the EFD_NONBLOCK fd (the kernel honours the flag — see
		 * fd_nonblock in servers/vfs) rather than block; skipping it is a
		 * no-op either way. Callers that poll get_fd() correctly see it
		 * stay unreadable (counter always 0). */
		return 0;
	}

	/* VT path: get_fd() returned vt_fd (VT0_PATH), so the caller believes it
	 * is readable. Drain the notification byte first -- the read itself is
	 * what clears this open's edge state, so it must happen even though the
	 * payload isn't the thing trusted below. EAGAIN means the fd was added
	 * to the caller's event loop but nothing is pending yet (e.g. a
	 * spurious wakeup, or dispatch() called speculatively); that is "0
	 * messages processed", not an error. Any other read() failure is
	 * treated the same way defensively. */
	unsigned char notif_byte = 0;
	ssize_t n = read(seat->vt_fd, &notif_byte, sizeof(notif_byte));
	if (n < 0) {
		if (errno != EAGAIN && errno != EWOULDBLOCK) {
			trc("dispatch seat=%p vt_fd read failed errno=%d, treating as no messages",
			    (void *)seat, errno);
		}
		return 0;
	}
	if (n == 0) {
		/* EOF from a device node isn't expected, but this whole path is
		 * still new; don't turn an unexpected condition into a hard error. */
		trc("dispatch seat=%p vt_fd read returned EOF, treating as no messages", (void *)seat);
		return 0;
	}

	/* Re-query with VT_GETSTATE instead of trusting notif_byte. The byte
	 * already carries the new active VT number, so this is a deliberate
	 * choice, not a missed shortcut: it is the difference between tracking
	 * *state* and tracking *events*. A query is idempotent and self-healing
	 * -- if the kernel coalesces several switches into one wakeup, or this
	 * process is scheduled late and misses a wakeup entirely, VT_GETSTATE
	 * still returns the true current state, while the byte only ever tells
	 * us where the VT was at the moment of that one read. Trusting the byte
	 * would mean two different code paths compute "am I active" (a
	 * one-shot read here vs. VT_GETSTATE in vt_probe() at open time), which
	 * is exactly the kind of drift that produces a compositor convinced
	 * it's active on the wrong VT -- worse than the always-active fallback,
	 * per the correctness requirement this whole feature exists to satisfy.
	 * One ioctl per human-triggered VT switch is not a cost worth avoiding
	 * for that. */
	struct vt_stat st;
	memset(&st, 0, sizeof(st));
	if (ioctl(seat->vt_fd, VT_GETSTATE, &st) != 0) {
		trc("dispatch seat=%p vt_fd VT_GETSTATE failed errno=%d, treating as no messages",
		    (void *)seat, errno);
		return 0;
	}
	if ((unsigned)notif_byte != (unsigned)st.v_active) {
		/* Informational only: expected to happen occasionally under rapid
		 * switching (the byte was already stale by the time we re-queried),
		 * never a reason to distrust VT_GETSTATE. */
		trc("dispatch seat=%p notif_byte=%u != VT_GETSTATE v_active=%u (stale notification, "
		    "using VT_GETSTATE)",
		    (void *)seat, (unsigned)notif_byte, (unsigned)st.v_active);
	}

	int new_active = (st.v_active == (unsigned short)seat->own_vtnr) ? 1 : 0;
	if (new_active == seat->active) {
		trc("dispatch seat=%p vt notification with no state change (active=%d, v_active=%u, "
		    "own_vtnr=%d)",
		    (void *)seat, seat->active, (unsigned)st.v_active, seat->own_vtnr);
		return 0;
	}

	seat->active = new_active;
	if (new_active) {
		trc("dispatch seat=%p -> enable_seat (VT %d became foreground)", (void *)seat,
		    seat->own_vtnr);
		seat->listener->enable_seat(seat, seat->userdata);
	} else {
		trc("dispatch seat=%p -> disable_seat (VT %d lost foreground to %u)", (void *)seat,
		    seat->own_vtnr, (unsigned)st.v_active);
		seat->listener->disable_seat(seat, seat->userdata);
	}
	return 1;
}
