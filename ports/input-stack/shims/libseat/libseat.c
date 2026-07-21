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
		return NULL;
	}

	struct libseat *seat = calloc(1, sizeof(*seat));
	if (seat == NULL) {
		errno = ENOMEM;
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
		return NULL;
	}

	/* Deliver activation immediately, per the D3 contract. */
	seat->listener->enable_seat(seat, seat->userdata);
	return seat;
}

int libseat_close_seat(struct libseat *seat) {
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
		return NULL;
	}
	return SEAT_NAME;
}

/* -------- device open/close -------- */

int libseat_open_device(struct libseat *seat, const char *path, int *fd) {
	if (seat == NULL || path == NULL || fd == NULL) {
		errno = EINVAL;
		return -1;
	}
	/* Running as root with direct access: just open the node. The returned
	 * device id is the fd itself (close_device takes the id back). */
	int f = open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
	if (f < 0) {
		return -1; /* open() set errno */
	}
	*fd = f;
	return f; /* device id == fd */
}

int libseat_close_device(struct libseat *seat, int device_id) {
	if (seat == NULL || device_id < 0) {
		errno = EINVAL;
		return -1;
	}
	if (close(device_id) < 0) {
		return -1; /* close() set errno */
	}
	return 0;
}

/* -------- session switching (unsupported: single session) -------- */

int libseat_switch_session(struct libseat *seat, int session) {
	/* No VT / multi-session support. A switch request is silently accepted
	 * and has no effect, exactly as the API permits ("does not imply that a
	 * switch will occur"). */
	(void)session;
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
		return -1;
	}
	return seat->conn_fd;
}

int libseat_dispatch(struct libseat *seat, int timeout) {
	/* No backend messages ever arrive. Drain the eventfd in case a caller
	 * ever writes to it, then report "0 messages processed" (success). We
	 * do not sleep for `timeout`: there is by construction nothing to wait
	 * for, and blocking here would only stall the caller's event loop. */
	(void)timeout;
	if (seat == NULL) {
		errno = EINVAL;
		return -1;
	}
	uint64_t drain;
	while (read(seat->conn_fd, &drain, sizeof(drain)) == (ssize_t)sizeof(drain)) {
		/* discard */
	}
	return 0;
}
