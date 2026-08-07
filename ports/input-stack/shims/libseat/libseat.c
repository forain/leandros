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
 *   - Exactly one seat, named "seat0", always active.
 *   - The process is assumed to already have permission to open DRM and evdev
 *     nodes (it runs as root), so open_device() is a plain open() — no
 *     privileged helper, no drmSetMaster brokering (the compositor calls
 *     DRM SET_MASTER itself once it holds the fd).
 *   - No session switching: switch_session() and disable_seat() are no-ops
 *     that report success. The seat is never revoked, so the disable_seat
 *     listener callback is never emitted.
 *   - enable_seat is delivered exactly once, synchronously, from
 *     libseat_open_seat() — matching the plan's D3 contract and the fact that
 *     real libseat may also call back synchronously during open. Callers
 *     (smithay's libseat-rs) install their userdata before the open call
 *     precisely to tolerate this.
 *   - get_fd() returns a real, pollable eventfd that is never signalled, so a
 *     caller can register it in its event loop and it simply never wakes.
 *     dispatch() therefore always reports "0 messages processed".
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

#define SEAT_NAME "seat0"

struct libseat {
	const struct libseat_seat_listener *listener;
	void *userdata;
	int conn_fd; /* eventfd; pollable, never signalled */
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

	/* A pollable fd that never becomes readable: the caller can add it to
	 * its event loop; it simply never fires (QEMU device set is fixed). */
	seat->conn_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
	if (seat->conn_fd < 0) {
		int e = errno;
		free(seat);
		errno = e;
		trc("open_seat listener=%p -> seat=(nil) fd=-1", (const void *)listener);
		return NULL;
	}

	/* Deliver activation immediately, per the D3 contract. */
	seat->listener->enable_seat(seat, seat->userdata);
	trc("open_seat listener=%p -> seat=%p fd=%d", (const void *)listener, (void *)seat, seat->conn_fd);
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
	free(seat);
	return 0;
}

int libseat_disable_seat(struct libseat *seat) {
	/* We never emit a disable_seat event, so acknowledgement is a no-op.
	 * Report success so callers that defensively call this still proceed. */
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
	trc("get_fd seat=%p -> %d", (void *)seat, seat->conn_fd);
	return seat->conn_fd;
}

int libseat_dispatch(struct libseat *seat, int timeout) {
	/* No backend messages ever arrive (no seatd/logind connection), so there
	 * is nothing to process: report "0 messages processed" (success) without
	 * touching conn_fd. Nothing ever writes the eventfd counter, so a
	 * read(conn_fd) here would just return EAGAIN on the EFD_NONBLOCK fd
	 * (the kernel honours the flag — see fd_nonblock in servers/vfs) rather
	 * than block; skipping it is a no-op either way. Callers that poll
	 * get_fd() correctly see it stay unreadable (counter always 0).
	 *
	 * This is typically called once per event-loop iteration, so it is the
	 * dominant consumer of the trace budget; the shared budget/exhaustion
	 * line (SEAT_TRACE_BUDGET, in trc()) covers it like every other call. */
	if (seat == NULL) {
		errno = EINVAL;
		trc("dispatch seat=(nil) timeout=%d", timeout);
		return -1;
	}
	trc("dispatch seat=%p timeout=%d", (void *)seat, timeout);
	return 0;
}
