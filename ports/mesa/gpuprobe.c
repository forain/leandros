/* gpuprobe — what GPU path can this LeandrOS guest render COSMIC on?
 *
 *   gpuprobe caps   one line of shell assignments describing /dev/dri/card0:
 *                     GPU_DRM_NAME=virtio_gpu GPU_CAPSETS=0x16 GPU_VENUS=1 GPU_VIRGL=1
 *                   Venus = capset 4, virgl = capset 1 or 2 (VIRTGPU_GETPARAM
 *                   SUPPORTED_CAPSET_IDs). Exit 0 even when there is no GPU
 *                   (all zeros), 1 only if card0 cannot be opened.
 *
 *   gpuprobe gl     create a GBM device on card0 and a surfaceless GLES
 *                   context exactly the way cosmic-comp's smithay backend does
 *                   (EGL_PLATFORM_GBM_KHR, no config), with the CURRENT
 *                   environment (MESA_LOADER_DRIVER_OVERRIDE, GALLIUM_DRIVER,
 *                   VK_ICD_FILENAMES, GBM_ALWAYS_SOFTWARE ...), and print
 *                     GL_RENDERER=<string>
 *                   Exit 0 on a hardware renderer, 2 on a software one
 *                   (softpipe / llvmpipe / swrast / lavapipe), 3 if no context
 *                   could be created at all. Also prints
 *                     GL_WGPU=1|0 GL_WGPU_ERR=0x....
 *                   whether wgpu-hal's GLES backend (iced's `wgpu` renderer in
 *                   the COSMIC clients) can get a context here: it asks for a
 *                   GLES 3 context with robust buffer access first and falls
 *                   back without it ONLY on EGL_BAD_ATTRIBUTE (egl.rs). Mesa
 *                   answers EGL_BAD_MATCH per spec on a driver without robust
 *                   access (virgl on ANGLE/Metal) unless it carries
 *                   ports/mesa/patches/0001-egl-robust-access-unsupported-is-
 *                   bad-attribute.patch, and then wgpu never gets a context.
 *
 * /bin/gpu-env runs both before every COSMIC launch (greeter and session) so a
 * software-rendered desktop can never happen silently.
 *
 * Built in the Alpine container by build-gpu-stack-alpine.sh against the same
 * Mesa it ships with; no DRM-master is taken and nothing is drawn. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <gbm.h>

#define CARD "/dev/dri/card0"

struct drm_version {
	int version_major, version_minor, version_patchlevel;
	size_t name_len; char *name;
	size_t date_len; char *date;
	size_t desc_len; char *desc;
};
struct drm_virtgpu_getparam { uint64_t param; uint64_t value; };
#define DRM_IOCTL_VERSION_ 0xC0406400u /* _IOWR('d', 0x00, 64 bytes) */
#define DRM_IOCTL_VIRTGPU_GETPARAM_ 0xC0106443u
#define VIRTGPU_PARAM_3D_FEATURES 1
#define VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS 7

static int caps(void)
{
	int fd = open(CARD, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		printf("GPU_DRM_NAME= GPU_CAPSETS=0x0 GPU_VENUS=0 GPU_VIRGL=0 GPU_ERR=open:%d\n", errno);
		return 1;
	}
	char name[64] = {0};
	struct drm_version v; memset(&v, 0, sizeof v);
	v.name = name; v.name_len = sizeof name - 1;
	if (ioctl(fd, DRM_IOCTL_VERSION_, &v) != 0) name[0] = 0;
	name[sizeof name - 1] = 0;
	for (char *p = name; *p; p++) if (*p <= ' ' || *p == '\'' || *p == '"') *p = '_';

	uint64_t mask = 0, three_d = 0;
	struct drm_virtgpu_getparam gp = { VIRTGPU_PARAM_SUPPORTED_CAPSET_IDS, (uint64_t)(uintptr_t)&mask };
	/* The kernel writes an int-sized value, as upstream does. */
	uint32_t m32 = 0, d32 = 0;
	gp.value = (uint64_t)(uintptr_t)&m32;
	if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM_, &gp) == 0) mask = m32;
	gp.param = VIRTGPU_PARAM_3D_FEATURES; gp.value = (uint64_t)(uintptr_t)&d32;
	if (ioctl(fd, DRM_IOCTL_VIRTGPU_GETPARAM_, &gp) == 0) three_d = d32;
	close(fd);
	int venus = (mask >> 4) & 1;
	int virgl = three_d && (((mask >> 1) & 1) || ((mask >> 2) & 1));
	printf("GPU_DRM_NAME=%s GPU_CAPSETS=0x%llx GPU_VENUS=%d GPU_VIRGL=%d\n",
	       name, (unsigned long long)mask, venus, virgl);
	return 0;
}

static int is_software(const char *r)
{
	static const char *const sw[] = { "softpipe", "llvmpipe", "swrast", "lavapipe", "Software Rasterizer" };
	for (unsigned i = 0; i < sizeof sw / sizeof sw[0]; i++)
		if (strstr(r, sw[i])) return 1;
	return 0;
}

#ifndef EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT
#define EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT 0x30BF
#endif

/* wgpu-hal 28 gles/egl.rs context creation, reduced to the part that decides
 * whether it gets a context at all: GLES 3 + robust access (core on EGL 1.5,
 * else the EXT attribute when advertised), retried without robustness only
 * after EGL_BAD_ATTRIBUTE. */
static void wgpu_probe(EGLDisplay dpy, EGLint maj, EGLint min)
{
	const char *ext = eglQueryString(dpy, EGL_EXTENSIONS);
	int has_ext = ext && strstr(ext, "EGL_EXT_create_context_robustness");
	int level = (maj > 1 || (maj == 1 && min >= 5)) ? 2 : (has_ext ? 1 : 0);
	EGLint err = EGL_SUCCESS;
	for (;;) {
		EGLint a[8], n = 0;
		a[n++] = EGL_CONTEXT_MAJOR_VERSION; a[n++] = 3;
		if (level == 2) { a[n++] = EGL_CONTEXT_OPENGL_ROBUST_ACCESS; a[n++] = EGL_TRUE; }
		if (level == 1) { a[n++] = EGL_CONTEXT_OPENGL_ROBUST_ACCESS_EXT; a[n++] = EGL_TRUE; }
		a[n++] = EGL_NONE;
		EGLContext c = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, a);
		if (c != EGL_NO_CONTEXT) {
			eglDestroyContext(dpy, c);
			printf("GL_WGPU=1 GL_WGPU_ROBUST=%d\n", level);
			return;
		}
		err = eglGetError();
		if (err == EGL_BAD_ATTRIBUTE && level == 2) { level = has_ext ? 1 : 0; continue; }
		if (err == EGL_BAD_ATTRIBUTE && level == 1) { level = 0; continue; }
		break;
	}
	printf("GL_WGPU=0 GL_WGPU_ERR=0x%x\n", err);
}

static int gl(void)
{
	int fd = open(CARD, O_RDWR | O_CLOEXEC);
	if (fd < 0) { printf("GL_RENDERER= GL_ERR=open:%d\n", errno); return 3; }
	struct gbm_device *gbm = gbm_create_device(fd);
	if (!gbm) { printf("GL_RENDERER= GL_ERR=gbm_create_device\n"); return 3; }
	PFNEGLGETPLATFORMDISPLAYEXTPROC get_dpy =
		(PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
	EGLDisplay dpy = get_dpy ? get_dpy(EGL_PLATFORM_GBM_KHR, gbm, NULL) : eglGetDisplay((EGLNativeDisplayType)gbm);
	EGLint maj, min;
	if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &maj, &min)) {
		printf("GL_RENDERER= GL_ERR=eglInitialize:0x%x\n", eglGetError()); return 3;
	}
	eglBindAPI(EGL_OPENGL_ES_API);
	static const EGLint attrs[] = { EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE };
	EGLContext ctx = eglCreateContext(dpy, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, attrs);
	if (ctx == EGL_NO_CONTEXT || !eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) {
		printf("GL_RENDERER= GL_ERR=context:0x%x\n", eglGetError()); return 3;
	}
	const char *r = (const char *)glGetString(GL_RENDERER);
	const char *ver = (const char *)glGetString(GL_VERSION);
	char buf[256];
	snprintf(buf, sizeof buf, "%s", r ? r : "");
	for (char *p = buf; *p; p++) if (*p == '\'' || *p == '\n') *p = ' ';
	printf("GL_RENDERER='%s'\n", buf);
	fprintf(stderr, "gpuprobe: %s | %s\n", r ? r : "(null)", ver ? ver : "(null)");
	int sw = !r || is_software(r);
	eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
	eglDestroyContext(dpy, ctx);
	wgpu_probe(dpy, maj, min);
	eglTerminate(dpy);
	gbm_device_destroy(gbm);
	close(fd);
	return sw ? 2 : 0;
}

int main(int argc, char **argv)
{
	if (argc == 2 && !strcmp(argv[1], "caps")) return caps();
	if (argc == 2 && !strcmp(argv[1], "gl")) return gl();
	fprintf(stderr, "usage: gpuprobe caps|gl\n");
	return 64;
}
