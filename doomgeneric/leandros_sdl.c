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
#include "doomgeneric.h"

// ── PipeWire / LeandrOS Audio Protocol ──────────────────────────────────────

extern uint32_t get_audio_port(void);
static uint32_t s_pw_port = 0xFFFFFFFF; 

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
    s_pw_port = get_audio_port();
    if (s_pw_port == 0xFFFFFFFF) {
        write(1, "[SDL]   PipeWire port not found in auxv\n", 40);
        return;
    }
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x1000; // PING
    long ret = syscall3(513, s_pw_port, (long)&msg, 0);
    if (ret == 0 && msg.tag == 0x1001) {
        write(1, "[SDL]   PipeWire verified via IPC\n", 34);
    } else {
        write(1, "[SDL]   PipeWire verification failed!\n", 37);
        s_pw_port = 0xFFFFFFFF;
    }
}

void leandros_audio_pump() {
    if (s_pw_port == 0xFFFFFFFF) return;
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x300; // PUMP
    syscall3(511, s_pw_port, (long)&msg, 0); // IPC_SEND
}

void leandros_audio_send_pcm(const void* data, size_t len) {
    if (s_pw_port == 0xFFFFFFFF) return;
    const uint8_t* ptr = (const uint8_t*)data;
    size_t remaining = len;
    while (remaining > 0) {
        struct ipc_msg msg __attribute__((aligned(8)));
        memset(&msg, 0, sizeof(msg));
        msg.tag = 0x200;
        uint16_t actual_len = remaining > 436 ? 436 : remaining;
        msg.data[0] = (uint8_t)(actual_len & 0xFF);
        msg.data[1] = (uint8_t)((actual_len >> 8) & 0xFF);
        memcpy(&msg.data[2], ptr, actual_len);
        
        // Use IPC_SEND (511) instead of IPC_CALL (513) to avoid blocking
        long ret = syscall3(511, s_pw_port, (long)&msg, 0); 
        if (ret == -11) { // EAGAIN (port queue full)
            leandros_audio_pump();
            usleep(1000); 
            continue; 
        }
        
        ptr += actual_len;
        remaining -= actual_len;
    }
}

void leandros_audio_set_params(int freq, int channels) {
    if (s_pw_port == 0xFFFFFFFF) return;
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
static uint32_t s_screen_width, s_screen_height, s_screen_pitch;

SDL_Window* SDL_CreateWindow(const char* title, int x, int y, int w, int h, uint32_t flags) {
    write(1, "[SDL] SDL_CreateWindow\n", 23);
    
    // Prefer /dev/fb0 for now as it's more reliable for direct writes
    s_drm_fd = open("/dev/fb0", O_RDWR, 0);
    if (s_drm_fd < 0) s_drm_fd = open("/dev/dri/card0", O_RDWR, 0);
    
    if (s_drm_fd >= 0) {
        uint32_t info[8];
        // 0x4600 = FBIOGET_VSCREENINFO
        if (ioctl(s_drm_fd, 0x4600, (unsigned long)info) == 0) {
            s_screen_width = info[0];
            s_screen_height = info[1];
            s_screen_pitch = info[7];
            if (s_screen_pitch == 0) s_screen_pitch = s_screen_width * 4;
            
            char buf[128];
            int n = snprintf(buf, sizeof(buf), "[SDL] Detected framebuffer: %dx%d pitch=%d\n", 
                             (int)s_screen_width, (int)s_screen_height, (int)s_screen_pitch);
            write(1, buf, n);
        } else {
            s_screen_width = 320;
            s_screen_height = 200;
            s_screen_pitch = 320 * 4;
        }
    }
    
    return (SDL_Window*)0x1234;
}

SDL_Renderer* SDL_CreateRenderer(SDL_Window* window, int index, uint32_t flags) { return (SDL_Renderer*)0x5678; }
SDL_Texture* SDL_CreateTexture(SDL_Renderer* renderer, uint32_t format, int access, int w, int h) { return (SDL_Texture*)0x9ABC; }

extern void leandros_audio_pump(void);

void SDL_UpdateTexture(SDL_Texture* texture, const void* rect, const void* pixels, int pitch) {
    leandros_audio_pump();
    if (s_drm_fd >= 0 && pixels) {
        // If resolution matches exactly, use fast direct write
        if (s_screen_width == DOOMGENERIC_RESX && s_screen_height == DOOMGENERIC_RESY && s_screen_pitch == DOOMGENERIC_RESX * 4) {
            lseek(s_drm_fd, 0, SEEK_SET);
            write(s_drm_fd, pixels, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);
        } else {
            // Software scaling fallback
            static uint32_t* scaled_buffer = NULL;
            static int* x_map = NULL;
            static uint32_t last_width = 0, last_height = 0;
            
            if (scaled_buffer == NULL || last_width != s_screen_width || last_height != s_screen_height) {
                if (scaled_buffer) free(scaled_buffer);
                if (x_map) free(x_map);
                
                scaled_buffer = malloc(s_screen_width * s_screen_height * 4);
                x_map = malloc(s_screen_width * sizeof(int));
                
                if (x_map) {
                    for (int x = 0; x < s_screen_width; x++) {
                        x_map[x] = x * DOOMGENERIC_RESX / s_screen_width;
                    }
                }
                last_width = s_screen_width;
                last_height = s_screen_height;
            }
            
            if (!scaled_buffer || !x_map) return;

            const uint32_t* src_pixels = (const uint32_t*)pixels;

            for (int y = 0; y < s_screen_height; y++) {
                int src_y = y * DOOMGENERIC_RESY / s_screen_height;
                const uint32_t* src_row = &src_pixels[src_y * DOOMGENERIC_RESX];
                uint32_t* dest_row = &scaled_buffer[y * s_screen_width];
                
                for (int x = 0; x < s_screen_width; x++) {
                    dest_row[x] = src_row[x_map[x]];
                }
            }
            
            lseek(s_drm_fd, 0, SEEK_SET);
            write(s_drm_fd, scaled_buffer, s_screen_width * s_screen_height * 4);
        }
    }
}

void SDL_RenderClear(SDL_Renderer* renderer) {}
void SDL_RenderCopy(SDL_Renderer* renderer, SDL_Texture* texture, const void* srcrect, const void* dstrect) {}
void SDL_RenderPresent(SDL_Renderer* renderer) {}
int SDL_PollEvent(SDL_Event* event) {
    leandros_audio_pump();
    static int ev_fd = -2;
    if (ev_fd == -2) {
        ev_fd = open("/dev/input/event0", O_RDONLY, 0); 
    }
    
    if (ev_fd < 0) return 0;

    int bytes_available = 0;
    if (ioctl(ev_fd, FIONREAD, &bytes_available) < 0 || bytes_available < 1) {
        return 0;
    }

    struct {
        uint64_t sec, usec;
        uint16_t type, code;
        int32_t value;
    } ev;

    if (read(ev_fd, &ev, sizeof(ev)) == sizeof(ev)) {
        if (ev.type == 1) { // EV_KEY
            event->type = (ev.value == 0) ? SDL_KEYUP : SDL_KEYDOWN;
            
            // Linux evdev codes to SDL keycodes.
            uint32_t sym = 0;
            switch (ev.code) {
                case 1:   sym = SDLK_ESCAPE; break;
                case 2:   sym = '1'; break;
                case 3:   sym = '2'; break;
                case 4:   sym = '3'; break;
                case 5:   sym = '4'; break;
                case 6:   sym = '5'; break;
                case 7:   sym = '6'; break;
                case 8:   sym = '7'; break;
                case 9:   sym = '8'; break;
                case 10:  sym = '9'; break;
                case 11:  sym = '0'; break;
                case 12:  sym = SDLK_MINUS; break;
                case 13:  sym = SDLK_EQUALS; break;
                case 14:  sym = SDLK_BACKSPACE; break;
                case 15:  sym = SDLK_TAB; break;
                case 16:  sym = 'q'; break;
                case 17:  sym = 'w'; break;
                case 18:  sym = 'e'; break;
                case 19:  sym = 'r'; break;
                case 20:  sym = 't'; break;
                case 21:  sym = 'y'; break;
                case 22:  sym = 'u'; break;
                case 23:  sym = 'i'; break;
                case 24:  sym = 'o'; break;
                case 25:  sym = 'p'; break;
                case 26:  sym = SDLK_LEFTBRACKET; break;
                case 27:  sym = SDLK_RIGHTBRACKET; break;
                case 28:  sym = SDLK_RETURN; break;
                case 29:  sym = SDLK_LCTRL; break;
                case 30:  sym = 'a'; break;
                case 31:  sym = 's'; break;
                case 32:  sym = 'd'; break;
                case 33:  sym = 'f'; break;
                case 34:  sym = 'g'; break;
                case 35:  sym = 'h'; break;
                case 36:  sym = 'j'; break;
                case 37:  sym = 'k'; break;
                case 38:  sym = 'l'; break;
                case 39:  sym = SDLK_SEMICOLON; break;
                case 40:  sym = SDLK_QUOTE; break;
                case 41:  sym = SDLK_BACKQUOTE; break;
                case 42:  sym = SDLK_LSHIFT; break;
                case 43:  sym = SDLK_BACKSLASH; break;
                case 44:  sym = 'z'; break;
                case 45:  sym = 'x'; break;
                case 46:  sym = 'c'; break;
                case 47:  sym = 'v'; break;
                case 48:  sym = 'b'; break;
                case 49:  sym = 'n'; break;
                case 50:  sym = 'm'; break;
                case 51:  sym = SDLK_COMMA; break;
                case 52:  sym = SDLK_PERIOD; break;
                case 53:  sym = SDLK_SLASH; break;
                case 54:  sym = SDLK_RSHIFT; break;
                case 55:  sym = SDLK_KP_MULTIPLY; break;
                case 56:  sym = SDLK_LALT; break;
                case 57:  sym = SDLK_SPACE; break;
                case 58:  sym = SDLK_CAPSLOCK; break;
                case 59:  sym = SDLK_F1; break;
                case 60:  sym = SDLK_F2; break;
                case 61:  sym = SDLK_F3; break;
                case 62:  sym = SDLK_F4; break;
                case 63:  sym = SDLK_F5; break;
                case 64:  sym = SDLK_F6; break;
                case 65:  sym = SDLK_F7; break;
                case 66:  sym = SDLK_F8; break;
                case 67:  sym = SDLK_F9; break;
                case 68:  sym = SDLK_F10; break;
                case 69:  sym = SDLK_NUMLOCKCLEAR; break;
                case 70:  sym = SDLK_SCROLLLOCK; break;
                case 71:  sym = SDLK_KP_7; break;
                case 72:  sym = SDLK_KP_8; break;
                case 73:  sym = SDLK_KP_9; break;
                case 74:  sym = SDLK_KP_MINUS; break;
                case 75:  sym = SDLK_KP_4; break;
                case 76:  sym = SDLK_KP_5; break;
                case 77:  sym = SDLK_KP_6; break;
                case 78:  sym = SDLK_KP_PLUS; break;
                case 79:  sym = SDLK_KP_1; break;
                case 80:  sym = SDLK_KP_2; break;
                case 81:  sym = SDLK_KP_3; break;
                case 82:  sym = SDLK_KP_0; break;
                case 83:  sym = SDLK_KP_PERIOD; break;
                case 87:  sym = SDLK_F11; break;
                case 88:  sym = SDLK_F12; break;
                case 96:  sym = SDLK_KP_ENTER; break;
                case 97:  sym = SDLK_RCTRL; break;
                case 98:  sym = SDLK_KP_DIVIDE; break;
                case 100: sym = SDLK_RALT; break;
                case 102: sym = SDLK_HOME; break;
                case 103: sym = SDLK_UP; break;
                case 104: sym = SDLK_PAGEUP; break;
                case 105: sym = SDLK_LEFT; break;
                case 106: sym = SDLK_RIGHT; break;
                case 107: sym = SDLK_END; break;
                case 108: sym = SDLK_DOWN; break;
                case 109: sym = SDLK_PAGEDOWN; break;
                case 110: sym = SDLK_INSERT; break;
                case 111: sym = SDLK_DELETE; break;
                
                default: sym = 0; break; 
            }
            if (sym == 0) return 0;
            event->key.keysym.sym = sym;
            return 1;
        }
    }

    return 0;
}

#define CLOCK_MONOTONIC 1
uint32_t SDL_GetTicks(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (ts.tv_sec * 1000) + (ts.tv_nsec / 1000000);
}

void SDL_Delay(uint32_t ms) { usleep(ms * 1000); }
void SDL_SetWindowTitle(SDL_Window* window, const char* title) {}
void SDL_Quit(void) { if (s_drm_fd >= 0) close(s_drm_fd); s_drm_fd = -1; }
