/* es31test - GLES 3.1 compute smoke test (V3D SUBMIT_CSD end to end).
 *
 * EGL on GBM (surfaceless context), SSBO of 4096 u32, local_size_x=64,
 * dispatch (64,1,1); each invocation writes 0xC5D00000 | id. Every word is
 * checked after glMemoryBarrier + map. One greppable result line:
 *   es31test PASS csd ok 4096/4096 renderer=<GL_RENDERER>
 *   es31test FAIL <reason> ...
 * With --fbo: render a flat red quad (pixel centres [64,192)) over a blue
 * clear into a 256x256 RGBA8 FBO, glReadPixels, compare exactly with the
 * analytic image, print
 *   es31test PASS fbo crc=0x........ bad=0/65536 renderer=<GL_RENDERER>
 * Usage: es31test [/dev/dri/card0] [--want V3D] [--fbo]   (--want: substring the
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

static unsigned crc32(const unsigned char *p, unsigned n)
{
	unsigned c = ~0u;
	while (n--) {
		c ^= *p++;
		for (int k = 0; k < 8; k++) c = (c >> 1) ^ (0xEDB88320u & -(c & 1));
	}
	return ~c;
}

static GLuint mkshader(GLenum t, const char *src)
{
	GLuint s = glCreateShader(t);
	glShaderSource(s, 1, &src, NULL);
	glCompileShader(s);
	GLint ok = 0;
	glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
	if (!ok) { char log[256] = ""; glGetShaderInfoLog(s, sizeof log, NULL, log); printf("# %s\n", log); return 0; }
	return s;
}

static int fbo_test(const char *rbuf)
{
	enum { W = 256, H = 256 };
	static unsigned char px[W * H * 4];
	GLuint rb, fb;
	glGenRenderbuffers(1, &rb);
	glBindRenderbuffer(GL_RENDERBUFFER, rb);
	glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, W, H);
	glGenFramebuffers(1, &fb);
	glBindFramebuffer(GL_FRAMEBUFFER, fb);
	glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
	GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
	if (st != GL_FRAMEBUFFER_COMPLETE) { printf("es31test FAIL fbo-incomplete 0x%x %s\n", st, rbuf); return 1; }
	glViewport(0, 0, W, H);
	glDisable(GL_BLEND); glDisable(GL_DEPTH_TEST); glDisable(GL_CULL_FACE); glDisable(GL_DITHER);
	glClearColor(0.f, 0.f, 1.f, 1.f);
	glClear(GL_COLOR_BUFFER_BIT);
	GLuint vs = mkshader(GL_VERTEX_SHADER, "#version 300 es\nlayout(location=0) in vec2 p;\nvoid main(){gl_Position=vec4(p,0.,1.);}\n");
	GLuint fs = mkshader(GL_FRAGMENT_SHADER, "#version 300 es\nprecision mediump float;\nout vec4 c;\nvoid main(){c=vec4(1.,0.,0.,1.);}\n");
	if (!vs || !fs) return printf("es31test FAIL fbo-compile %s\n", rbuf), 1;
	GLuint prog = glCreateProgram();
	glAttachShader(prog, vs); glAttachShader(prog, fs);
	glLinkProgram(prog);
	GLint ok = 0;
	glGetProgramiv(prog, GL_LINK_STATUS, &ok);
	if (!ok) return printf("es31test FAIL fbo-link %s\n", rbuf), 1;
	glUseProgram(prog);
	static const float quad[] = { -.5f, -.5f, .5f, -.5f, -.5f, .5f, .5f, .5f };
	GLuint vb;
	glGenBuffers(1, &vb);
	glBindBuffer(GL_ARRAY_BUFFER, vb);
	glBufferData(GL_ARRAY_BUFFER, sizeof quad, quad, GL_STATIC_DRAW);
	glEnableVertexAttribArray(0);
	glVertexAttribPointer(0, 2, GL_FLOAT, GL_FALSE, 0, 0);
	glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
	glFinish();
	glReadPixels(0, 0, W, H, GL_RGBA, GL_UNSIGNED_BYTE, px);
	GLenum err = glGetError();
	unsigned bad = 0, first = W * H;
	for (unsigned y = 0; y < H; y++) for (unsigned x = 0; x < W; x++) {
		int in = x >= 64 && x < 192 && y >= 64 && y < 192;
		const unsigned char *q = &px[(y * W + x) * 4];
		int good = in ? (q[0] == 255 && q[1] == 0 && q[2] == 0 && q[3] == 255)
			      : (q[0] == 0 && q[1] == 0 && q[2] == 255 && q[3] == 255);
		if (!good) { bad++; if (first == W * H) first = y * W + x; }
	}
	unsigned crc = crc32(px, sizeof px);
	if (bad) {
		const unsigned char *q = &px[first * 4];
		printf("es31test FAIL fbo crc=0x%08x bad=%u/%u first=(%u,%u) rgba=%u,%u,%u,%u glerr=0x%x %s\n",
		       crc, bad, W * H, first % W, first / W, q[0], q[1], q[2], q[3], err, rbuf);
		return 1;
	}
	printf("es31test PASS fbo crc=0x%08x bad=0/%u %s\n", crc, W * H, rbuf);
	return 0;
}

static int fail(const char *why, const char *extra)
{
	printf("es31test FAIL %s %s\n", why, extra ? extra : "");
	return 1;
}

int main(int argc, char **argv)
{
	const char *node = "/dev/dri/card0", *want = NULL;
	int fbo = 0;
	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--want") && i + 1 < argc) want = argv[++i];
		else if (!strcmp(argv[i], "--fbo")) fbo = 1;
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

	if (fbo) return fbo_test(rbuf);

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
