/* es31test - GLES 3.1 compute smoke test (V3D SUBMIT_CSD end to end).
 *
 * EGL on GBM (surfaceless context), SSBO of 4096 u32, local_size_x=64,
 * dispatch (64,1,1); each invocation writes 0xC5D00000 | id. Every word is
 * checked after glMemoryBarrier + map. One greppable result line:
 *   es31test PASS csd ok 4096/4096 renderer=<GL_RENDERER>
 *   es31test FAIL <reason> ...
 * Usage: es31test [/dev/dri/card0] [--want V3D]   (--want: substring the
 * GL_RENDERER must contain, else FAIL; default: any renderer passes).
 */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <fcntl.h>
#include <gbm.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define N 4096u

static const char *cs_src =
	"#version 310 es\n"
	"layout(local_size_x = 64) in;\n"
	"layout(std430, binding = 0) buffer Out { uint data[]; };\n"
	"void main() { uint id = gl_GlobalInvocationID.x; data[id] = 0xC5D00000u | id; }\n";

static int fail(const char *why, const char *extra)
{
	printf("es31test FAIL %s %s\n", why, extra ? extra : "");
	return 1;
}

int main(int argc, char **argv)
{
	const char *node = "/dev/dri/card0", *want = NULL;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--want") && i + 1 < argc) want = argv[++i];
		else node = argv[i];
	}
	int fd = open(node, O_RDWR | O_CLOEXEC);
	if (fd < 0) return fail("open", node);
	struct gbm_device *gbm = gbm_create_device(fd);
	if (!gbm) return fail("gbm_create_device", node);
	PFNEGLGETPLATFORMDISPLAYEXTPROC gpd =
		(PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
	EGLDisplay dpy = gpd ? gpd(EGL_PLATFORM_GBM_KHR, gbm, NULL) : EGL_NO_DISPLAY;
	EGLint maj, min;
	if (dpy == EGL_NO_DISPLAY || !eglInitialize(dpy, &maj, &min)) return fail("eglInitialize", NULL);
	if (!eglBindAPI(EGL_OPENGL_ES_API)) return fail("eglBindAPI", NULL);
	static const EGLint cfg_attr[] = { EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT_KHR, EGL_NONE };
	EGLConfig cfg; EGLint n = 0;
	if (!eglChooseConfig(dpy, cfg_attr, &cfg, 1, &n) || n < 1) return fail("eglChooseConfig", NULL);
	static const EGLint ctx_attr[] = { EGL_CONTEXT_MAJOR_VERSION, 3, EGL_CONTEXT_MINOR_VERSION, 1, EGL_NONE };
	EGLContext ctx = eglCreateContext(dpy, cfg, EGL_NO_CONTEXT, ctx_attr);
	if (ctx == EGL_NO_CONTEXT) return fail("eglCreateContext(3.1)", NULL);
	if (!eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx)) return fail("eglMakeCurrent(surfaceless)", NULL);
	const char *rend = (const char *)glGetString(GL_RENDERER);
	char rbuf[128];
	snprintf(rbuf, sizeof rbuf, "renderer=%s", rend ? rend : "?");
	for (char *p = rbuf; *p; p++) if (*p == ' ') *p = '_';
	if (want && (!rend || !strstr(rend, want))) return fail("wrong-renderer", rbuf);

	GLuint sh = glCreateShader(GL_COMPUTE_SHADER);
	glShaderSource(sh, 1, &cs_src, NULL);
	glCompileShader(sh);
	GLint ok = 0;
	glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
	if (!ok) { char log[256] = ""; glGetShaderInfoLog(sh, sizeof log, NULL, log); printf("# %s\n", log); return fail("compile", rbuf); }
	GLuint prog = glCreateProgram();
	glAttachShader(prog, sh);
	glLinkProgram(prog);
	glGetProgramiv(prog, GL_LINK_STATUS, &ok);
	if (!ok) return fail("link", rbuf);

	GLuint buf;
	glGenBuffers(1, &buf);
	glBindBuffer(GL_SHADER_STORAGE_BUFFER, buf);
	glBufferData(GL_SHADER_STORAGE_BUFFER, N * 4, NULL, GL_DYNAMIC_READ);
	void *z = glMapBufferRange(GL_SHADER_STORAGE_BUFFER, 0, N * 4, GL_MAP_WRITE_BIT | GL_MAP_INVALIDATE_BUFFER_BIT);
	if (!z) return fail("map-zero", rbuf);
	memset(z, 0, N * 4);
	glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
	glBindBufferBase(GL_SHADER_STORAGE_BUFFER, 0, buf);
	glUseProgram(prog);
	glDispatchCompute(N / 64, 1, 1);
	glMemoryBarrier(GL_BUFFER_UPDATE_BARRIER_BIT | GL_SHADER_STORAGE_BARRIER_BIT);
	glFinish();
	GLenum err = glGetError();
	const unsigned *d = glMapBufferRange(GL_SHADER_STORAGE_BUFFER, 0, N * 4, GL_MAP_READ_BIT);
	if (!d) return fail("map-read", rbuf);
	unsigned good = 0, first_bad = N;
	for (unsigned i = 0; i < N; i++) {
		if (d[i] == (0xC5D00000u | i)) good++;
		else if (first_bad == N) first_bad = i;
	}
	if (good != N) {
		printf("es31test FAIL mismatch %u/%u first_bad=%u val=0x%08x glerr=0x%x %s\n",
		       good, N, first_bad, d[first_bad], err, rbuf);
		return 1;
	}
	glUnmapBuffer(GL_SHADER_STORAGE_BUFFER);
	printf("es31test PASS csd ok %u/%u %s\n", good, N, rbuf);
	eglMakeCurrent(dpy, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
	eglDestroyContext(dpy, ctx);
	eglTerminate(dpy);
	gbm_device_destroy(gbm);
	close(fd);
	return 0;
}
