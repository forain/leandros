#include <SDL.h>
#include <unistd.h>
#include <stdio.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <SDL_mixer.h>

// ── PipeWire / LeandrOS Audio Protocol ──────────────────────────────────────

static uint32_t s_pw_port = 3; 

struct ipc_msg {
    uint64_t tag;         // 0
    uint32_t reply_port;  // 8
    uint8_t  data[440];   // 12
    uint32_t _padding;    // 452
    uint64_t has_cap;     // 456
    uint64_t cap;         // 464
};

// ── Syscall Wrappers ────────────────────────────────────────────────────────

static long syscall3(long num, long a0, long a1, long a2) {
    long ret;
    #ifdef __aarch64__
    register long r8 __asm__("x8") = num;
    register long r0 __asm__("x0") = a0;
    register long r1 __asm__("x1") = a1;
    register long r2 __asm__("x2") = a2;
    __asm__ __volatile__("svc #0" : "=r"(r0) : "r"(r8), "r"(r0), "r"(r1), "r"(r2) : "memory");
    ret = r0;
    #else
    __asm__ __volatile__("syscall" : "=a"(ret) : "a"(num), "D"(a0), "S"(a1), "d"(a2) : "rcx", "r11", "memory");
    #endif
    return ret;
}

void leandros_audio_init() {
    write(1, "[SDL] leandros_audio_init\n", 26);
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x1000; // PING
    long ret = syscall3(513, s_pw_port, (long)&msg, 0);
    if (ret == 0 && msg.tag == 0x1001) {
        write(1, "[SDL]   PipeWire verified on port 3\n", 36);
    } else {
        write(1, "[SDL]   PipeWire verification failed!\n", 37);
    }
}

void leandros_audio_send_pcm(const void* data, size_t len) {
    const uint8_t* ptr = (const uint8_t*)data;
    size_t remaining = len;
    while (remaining > 0) {
        struct ipc_msg msg __attribute__((aligned(8)));
        memset(&msg, 0, sizeof(msg));
        msg.tag = 0x200;
        uint16_t actual_len = remaining > 438 ? 438 : remaining;
        msg.data[0] = (uint8_t)(actual_len & 0xFF);
        msg.data[1] = (uint8_t)((actual_len >> 8) & 0xFF);
        memcpy(&msg.data[2], ptr, actual_len);
        syscall3(513, s_pw_port, (long)&msg, 0); 
        ptr += actual_len;
        remaining -= actual_len;
    }
}

void leandros_audio_set_params(int freq, int channels) {
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x100; // SET_PARAMS
    msg.data[0] = (uint8_t)(freq & 0xFF);
    msg.data[1] = (uint8_t)((freq >> 8) & 0xFF);
    msg.data[2] = (uint8_t)((freq >> 16) & 0xFF);
    msg.data[3] = (uint8_t)((freq >> 24) & 0xFF);
    msg.data[4] = (uint8_t)channels;
    syscall3(513, s_pw_port, (long)&msg, 0);
}

// ── SDL 3 Native Audio Stubs ────────────────────────────────────────────────

int SDL_Init(uint32_t flags) {
    write(1, "[SDL] SDL_Init\n", 15);
    if (flags & 0x00000010) leandros_audio_init();
    return 0;
}
int SDL_InitSubSystem(uint32_t flags) { return SDL_Init(flags); }
void SDL_QuitSubSystem(uint32_t flags) {}

// ── SDL_mixer Stubs ─────────────────────────────────────────────────────────

int Mix_OpenAudio(int frequency, uint16_t format, int channels, int chunksize) {
    write(1, "[SDL] Mix_OpenAudio\n", 20);
    leandros_audio_set_params(frequency, channels);
    return 0;
}
int Mix_AllocateChannels(int numchans) { return numchans; }
int Mix_Volume(int channel, int volume) { return volume; }
int Mix_PlayChannelTimed(int channel, Mix_Chunk *chunk, int loops, int ticks) {
    if (chunk && chunk->abuf) leandros_audio_send_pcm(chunk->abuf, chunk->alen);
    return channel;
}
void Mix_HaltChannel(int channel) {}
int Mix_Playing(int channel) { return 0; }
void Mix_CloseAudio(void) {}
int Mix_QuerySpec(int *frequency, uint16_t *format, int *channels) {
    if (frequency) *frequency = 44100;
    if (format) *format = 0x8010;
    if (channels) *channels = 2;
    return 1;
}
const SDL_version *Mix_Linked_Version(void) {
    static SDL_version v = {1, 2, 8};
    return &v;
}
const char *Mix_GetError(void) { return "No error"; }
int Mix_SetPanning(int channel, uint8_t left, uint8_t right) { return 1; }
int Mix_UnregisterAllEffects(int channel) { return 1; }
int Mix_HaltMusic(void) { return 0; }
int Mix_VolumeMusic(int volume) { return volume; }
int Mix_PlayMusic(Mix_Music *music, int loops) { return 0; }
void Mix_SetMusicCMD(const char *command) {}
int Mix_RegisterEffect(int chan, Mix_EffectFunc_t f, Mix_EffectDone_t d, void *arg) { return 0; }
void Mix_FreeMusic(Mix_Music *music) {}
Mix_Music *Mix_LoadMUS(const char *file) { return (Mix_Music *)0x12345678; }
int Mix_PlayingMusic(void) { return 0; }
int Mix_SetMusicPosition(double position) { return 0; }
void SDL_PauseAudio(int pause_on) {}
void SDL_LockAudio(void) {}
void SDL_UnlockAudio(void) {}

// ── Graphics Implementation (Simplified Fallback) ──────────────────────────

static int s_drm_fd = -1;

SDL_Window* SDL_CreateWindow(const char* title, int x, int y, int w, int h, uint32_t flags) {
    write(1, "[SDL] SDL_CreateWindow\n", 23);
    s_drm_fd = open("/dev/dri/card0", O_RDWR, 0);
    if (s_drm_fd < 0) s_drm_fd = open("/dev/fb0", O_RDWR, 0);
    return (SDL_Window*)0x1234;
}

SDL_Renderer* SDL_CreateRenderer(SDL_Window* window, int index, uint32_t flags) { return (SDL_Renderer*)0x5678; }
SDL_Texture* SDL_CreateTexture(SDL_Renderer* renderer, uint32_t format, int access, int w, int h) { return (SDL_Texture*)0x9ABC; }

void SDL_UpdateTexture(SDL_Texture* texture, const void* rect, const void* pixels, int pitch) {
    if (s_drm_fd >= 0 && pixels) {
        // Direct write() to framebuffer - slow but reliable for initial verification
        // VFS should handle this by copying to the physical FB.
        lseek(s_drm_fd, 0, SEEK_SET);
        write(s_drm_fd, pixels, 320 * 200 * 4);
    }
}

void SDL_RenderClear(SDL_Renderer* renderer) {}
void SDL_RenderCopy(SDL_Renderer* renderer, SDL_Texture* texture, const void* srcrect, const void* dstrect) {}
void SDL_RenderPresent(SDL_Renderer* renderer) {}
int SDL_PollEvent(SDL_Event* event) { return 0; }

#define CLOCK_MONOTONIC 1
uint32_t SDL_GetTicks(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (ts.tv_sec * 1000) + (ts.tv_nsec / 1000000);
}

void SDL_Delay(uint32_t ms) { usleep(ms * 1000); }
void SDL_SetWindowTitle(SDL_Window* window, const char* title) {}
void SDL_Quit(void) { if (s_drm_fd >= 0) close(s_drm_fd); s_drm_fd = -1; }
