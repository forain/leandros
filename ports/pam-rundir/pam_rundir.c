/* pam_rundir.so -- LeandrOS provider of the "pam_rundir" slot in Alpine's
 * /usr/lib/pam.d/base-session ("-session optional pam_rundir.so").
 *
 * Exports XDG_RUNTIME_DIR=/run/user/<uid of PAM_USER> into the PAM environment
 * when that directory exists (init / the runtime-dir setup creates one per
 * account), so su/sudo/greetd-started commands find the user's Wayland and
 * PipeWire sockets instead of the caller's. Session hooks only. */
#include <pwd.h>
#include <stdio.h>
#include <sys/stat.h>

/* Linux-PAM ABI, declared here so the build needs no PAM headers (the dynamic
 * sysroot's copies are the old shim's partial set). */
typedef struct pam_handle pam_handle_t;
#define PAM_SUCCESS 0
#define PAM_EXTERN extern
extern int pam_get_user(pam_handle_t *, const char **, const char *);
extern int pam_putenv(pam_handle_t *, const char *);

PAM_EXTERN int pam_sm_open_session(pam_handle_t *pamh, int flags, int argc, const char **argv)
{
	const char *user = NULL;
	struct passwd *pw;
	struct stat st;
	char dir[64], env[96];
	(void)flags; (void)argc; (void)argv;
	if (pam_get_user(pamh, &user, NULL) != PAM_SUCCESS || !user)
		return PAM_SUCCESS;
	pw = getpwnam(user);
	if (!pw)
		return PAM_SUCCESS;
	snprintf(dir, sizeof dir, "/run/user/%u", (unsigned)pw->pw_uid);
	if (stat(dir, &st) != 0 || !S_ISDIR(st.st_mode))
		return PAM_SUCCESS;
	snprintf(env, sizeof env, "XDG_RUNTIME_DIR=%s", dir);
	return pam_putenv(pamh, env);
}

PAM_EXTERN int pam_sm_close_session(pam_handle_t *pamh, int flags, int argc, const char **argv)
{
	(void)pamh; (void)flags; (void)argc; (void)argv;
	return PAM_SUCCESS;
}
