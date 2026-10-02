/* hostprobe-fan.c: does GL_TRIANGLE_FAN draw on the host GL that QEMU uses?
 * Renders a full-target quad as a fan, a strip, an indexed fan, an instanced fan
 * and a single triangle into an FBO, prints pixel counts and an ASCII map.
 * On ANGLE Vulkan / MoltenVK (Apple M4 Max) every fan covers exactly half
 * (its first triangle only); a correct host covers all of it.
 * Build and run on the Mac (as run-qemu.sh sets up MoltenVK):
 *   P=~/.local/qemu-gpu-gles31; I=~/.cache/leandros-qemu-gpu-gles31/angle/include
 *   cc -o fan hostprobe-fan.c -I$I -L$P/lib -lEGL -lGLESv2 -Wl,-rpath,$P/lib
 *   VK_DRIVER_FILES=/opt/homebrew/etc/vulkan/icd.d/MoltenVK_icd.json ANGLE_DEFAULT_PLATFORM=vulkan ./fan */
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl31.h>
#include <stdio.h>
#include <stdlib.h>
static GLuint sh(GLenum t,const char*s){GLuint h=glCreateShader(t);glShaderSource(h,1,&s,0);glCompileShader(h);GLint ok;glGetShaderiv(h,GL_COMPILE_STATUS,&ok);if(!ok){char b[999];glGetShaderInfoLog(h,999,0,b);puts(b);}return h;}
static void count(const char*tag,int W,int H){unsigned char*p=malloc(W*H*4);glReadPixels(0,0,W,H,GL_RGBA,GL_UNSIGNED_BYTE,p);
 int r=0,g=0,m=0,o=0;for(int i=0;i<W*H;i++){unsigned char*q=p+i*4;if(q[0]==255&&q[1]==0&&q[2]==0)r++;else if(q[0]==0&&q[1]==255&&q[2]==0)g++;else if(q[0]==255&&q[1]==0&&q[2]==255)m++;else o++;}
 printf("%-28s red=%d green=%d magenta=%d other=%d  (of %d) err=0x%x\n",tag,r,g,m,o,W*H,glGetError());
 // ascii map 16x8
 for(int y=H-1;y>=0;y-=H/8){for(int x=0;x<W;x+=W/16){unsigned char*q=p+(y*W+x)*4;putchar(q[0]==255&&q[1]==0&&q[2]==0?'R':q[1]==255&&q[0]==0?'G':q[0]==255&&q[2]==255&&q[1]==0?'M':'.');}putchar('\n');}
 free(p);}
int main(){
 EGLDisplay d=eglGetDisplay(EGL_DEFAULT_DISPLAY);EGLint a,b;if(!eglInitialize(d,&a,&b)){puts("init fail");return 1;}
 printf("EGL %d.%d vendor %s\n",a,b,eglQueryString(d,EGL_VENDOR));
 EGLint ca[]={EGL_SURFACE_TYPE,EGL_PBUFFER_BIT,EGL_RENDERABLE_TYPE,EGL_OPENGL_ES3_BIT,EGL_RED_SIZE,8,EGL_NONE};EGLConfig c;EGLint n;eglChooseConfig(d,ca,&c,1,&n);
 eglBindAPI(EGL_OPENGL_ES_API);EGLint cx[]={EGL_CONTEXT_MAJOR_VERSION,3,EGL_CONTEXT_MINOR_VERSION,1,EGL_NONE};
 EGLContext ctx=eglCreateContext(d,c,EGL_NO_CONTEXT,cx);if(!ctx){printf("ctx fail %x\n",eglGetError());return 1;}
 eglMakeCurrent(d,EGL_NO_SURFACE,EGL_NO_SURFACE,ctx);
 printf("GL_RENDERER %s\nGL_VERSION %s\n",glGetString(GL_RENDERER),glGetString(GL_VERSION));
 int W=256,H=256;GLuint tex,fb;glGenTextures(1,&tex);glBindTexture(GL_TEXTURE_2D,tex);glTexStorage2D(GL_TEXTURE_2D,1,GL_RGBA8,W,H);
 glGenFramebuffers(1,&fb);glBindFramebuffer(GL_FRAMEBUFFER,fb);glFramebufferTexture2D(GL_FRAMEBUFFER,GL_COLOR_ATTACHMENT0,GL_TEXTURE_2D,tex,0);
 glViewport(0,0,W,H);
 GLuint pr=glCreateProgram();glAttachShader(pr,sh(GL_VERTEX_SHADER,"#version 300 es\nin vec2 p;void main(){gl_Position=vec4(p,0,1);}"));
 glAttachShader(pr,sh(GL_FRAGMENT_SHADER,"#version 300 es\nprecision mediump float;out vec4 o;void main(){o=vec4(1,0,0,1);}"));glLinkProgram(pr);glUseProgram(pr);
 float v[]={-1,-1, 1,-1, 1,1, -1,1};GLuint vb;glGenBuffers(1,&vb);glBindBuffer(GL_ARRAY_BUFFER,vb);glBufferData(GL_ARRAY_BUFFER,sizeof v,v,GL_STATIC_DRAW);
 glEnableVertexAttribArray(0);glVertexAttribPointer(0,2,GL_FLOAT,0,0,0);
 count("uninit",W,H);
 glClearColor(0,1,0,1);glClear(GL_COLOR_BUFFER_BIT);glDrawArrays(GL_TRIANGLE_FAN,0,4);count("fan",W,H);
 glClear(GL_COLOR_BUFFER_BIT);glDrawArrays(GL_TRIANGLE_STRIP,0,4);count("strip (expect half)",W,H);
 GLushort idx[]={0,1,2,0,2,3};glClear(GL_COLOR_BUFFER_BIT);glDrawElements(GL_TRIANGLE_FAN,4,GL_UNSIGNED_SHORT,idx);count("fan indexed(client)",W,H);
 glClear(GL_COLOR_BUFFER_BIT);glDrawArraysInstanced(GL_TRIANGLE_FAN,0,4,1);count("fan instanced",W,H);
 glClear(GL_COLOR_BUFFER_BIT);glDrawArrays(GL_TRIANGLES,0,3);count("tri",W,H);
 return 0;}
