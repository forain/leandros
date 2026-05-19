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
    uint64_t tag;         
    uint32_t reply_port;  
    uint8_t  data[440];   
    uint32_t _padding;    
    uint64_t has_cap;     
    uint64_t cap;         
};

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

// ── Mixer and Synth State ───────────────────────────────────────────────────

#define MIX_CHANNELS 16
#define SAMPLE_RATE 44100

typedef struct {
    const uint8_t* data;
    uint32_t len;
    uint32_t pos;
    int active;
} mixer_channel_t;

struct Mix_Music {
    uint8_t* data;
    uint32_t len;
};

typedef struct {
    float freq;
    float phase;
    int active;
} synth_voice_t;

typedef struct {
    struct Mix_Music* music;
    uint32_t ptr;
    uint32_t division;
    uint32_t tempo; 
    uint32_t samples_left_in_delta;
    int looping;
    int active;
} seq_t;

static mixer_channel_t s_channels[MIX_CHANNELS];
static seq_t s_seq;
static synth_voice_t s_voices[16];
static uint32_t s_last_pump_ticks = 0;

// ── MIDI/Synth Logic ────────────────────────────────────────────────────────

static float get_note_freq(uint8_t note) {
    float f = 13.75f; 
    int note_in_octave = note % 12;
    int octave = (note / 12) - 1;
    static const float semi[] = {1.0f, 1.05946f, 1.12246f, 1.18921f, 1.25992f, 1.33484f, 1.41421f, 1.49831f, 1.5874f, 1.68179f, 1.7818f, 1.88775f};
    f *= semi[note_in_octave];
    if (octave > 0) for (int i=0; i<octave; i++) f *= 2.0f;
    else if (octave < 0) for (int i=0; i<-octave; i++) f /= 2.0f;
    return f;
}

static void synth_note_on(uint8_t note) {
    float f = get_note_freq(note);
    for (int i=0; i<16; i++) {
        if (!s_voices[i].active) {
            s_voices[i].freq = f;
            s_voices[i].phase = 0;
            s_voices[i].active = 1;
            break;
        }
    }
}

static void synth_note_off(uint8_t note) {
    float f = get_note_freq(note);
    for (int i=0; i<16; i++) {
        if (s_voices[i].active && (s_voices[i].freq - f) < 1.0f && (f - s_voices[i].freq) < 1.0f) {
            s_voices[i].active = 0;
        }
    }
}

static uint32_t read_varlen(const uint8_t* data, uint32_t* ptr) {
    uint32_t val = 0;
    while (1) {
        uint8_t b = data[(*ptr)++];
        val = (val << 7) | (b & 0x7F);
        if (!(b & 0x80)) break;
    }
    return val;
}

static void sequencer_step() {
    if (!s_seq.active || !s_seq.music) return;
    
    while (s_seq.samples_left_in_delta == 0) {
        uint8_t status = s_seq.music->data[s_seq.ptr++];
        switch (status & 0xF0) {
            case 0x80: synth_note_off(s_seq.music->data[s_seq.ptr++]); s_seq.ptr++; break;
            case 0x90: {
                uint8_t note = s_seq.music->data[s_seq.ptr++];
                uint8_t vel = s_seq.music->data[s_seq.ptr++];
                if (vel == 0) synth_note_off(note); else synth_note_on(note);
                break;
            }
            case 0xFF: {
                uint8_t t = s_seq.music->data[s_seq.ptr++];
                uint32_t l = read_varlen(s_seq.music->data, &s_seq.ptr);
                if (t == 0x51) {
                    s_seq.tempo = (s_seq.music->data[s_seq.ptr] << 16) | (s_seq.music->data[s_seq.ptr+1] << 8) | s_seq.music->data[s_seq.ptr+2];
                }
                s_seq.ptr += l;
                break;
            }
            case 0x2F: s_seq.active = 0; return; 
            default: { 
                if (status < 0x80) s_seq.ptr--; 
                else if (status < 0xC0 || status >= 0xE0) s_seq.ptr += 2;
                else s_seq.ptr += 1;
                break;
            }
        }
        if (s_seq.ptr >= s_seq.music->len) { s_seq.active = 0; return; }
        uint32_t delta = read_varlen(s_seq.music->data, &s_seq.ptr);
        s_seq.samples_left_in_delta = (uint32_t)((uint64_t)delta * s_seq.tempo * SAMPLE_RATE / (s_seq.division * 1000000));
    }
}

// ── Audio Core ──────────────────────────────────────────────────────────────

void leandros_audio_send_pcm(const void* data, size_t len) {
    if (s_pw_port == 0xFFFFFFFF) return;
    const uint8_t* ptr = (const uint8_t*)data;
    size_t remaining = len;
    while (remaining > 0) {
        struct ipc_msg msg __attribute__((aligned(8)));
        memset(&msg, 0, sizeof(msg));
        msg.tag = 0x200;
        msg.reply_port = 0xFFFFFFFF;
        uint16_t actual_len = remaining > 436 ? 436 : remaining;
        msg.data[0] = (uint8_t)(actual_len & 0xFF);
        msg.data[1] = (uint8_t)((actual_len >> 8) & 0xFF);
        memcpy(&msg.data[2], ptr, actual_len);
        syscall3(511, s_pw_port, (long)&msg, 0); 
        ptr += actual_len;
        remaining -= actual_len;
    }
}

void leandros_audio_pump() {
    if (s_pw_port == 0xFFFFFFFF) return;
    
    uint32_t now = SDL_GetTicks();
    if (s_last_pump_ticks == 0) { s_last_pump_ticks = now; return; }
    uint32_t dt_ms = now - s_last_pump_ticks;
    if (dt_ms < 10) return; 
    s_last_pump_ticks = now;

    uint32_t samples_to_mix = (SAMPLE_RATE * dt_ms) / 1000;
    if (samples_to_mix > 2048) samples_to_mix = 2048;

    int16_t mix_buf[2048];
    memset(mix_buf, 0, samples_to_mix * 2);

    for (uint32_t s=0; s<samples_to_mix; s++) {
        float sample = 0;
        if (s_seq.active) {
            if (s_seq.samples_left_in_delta > 0) s_seq.samples_left_in_delta--;
            else sequencer_step();

            int voices_active = 0;
            for (int v=0; v<16; v++) {
                if (s_voices[v].active) {
                    s_voices[v].phase += s_voices[v].freq / SAMPLE_RATE;
                    if (s_voices[v].phase > 1.0f) s_voices[v].phase -= 1.0f;
                    sample += (s_voices[v].phase < 0.5f ? 4000.0f : -4000.0f);
                    voices_active++;
                }
            }
            if (voices_active > 1) sample /= voices_active;
        }

        for (int c=0; c<MIX_CHANNELS; c++) {
            if (s_channels[c].active) {
                const int16_t* src = (const int16_t*)s_channels[c].data;
                sample += src[s_channels[c].pos++];
                if (s_channels[c].pos >= s_channels[c].len / 2) s_channels[c].active = 0;
            }
        }

        if (sample > 32767) sample = 32767;
        if (sample < -32768) sample = -32768;
        mix_buf[s] = (int16_t)sample;
    }

    int16_t stereo_buf[2048 * 2];
    for (uint32_t i=0; i<samples_to_mix; i++) {
        stereo_buf[i*2] = mix_buf[i];
        stereo_buf[i*2+1] = mix_buf[i];
    }
    leandros_audio_send_pcm(stereo_buf, samples_to_mix * 4);

    struct ipc_msg msg;
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x300; 
    msg.reply_port = 0xFFFFFFFF;
    syscall3(511, s_pw_port, (long)&msg, 0);
}

void leandros_audio_init() {
    write(1, "[SDL] leandros_audio_init\n", 26);
    s_pw_port = get_audio_port();
    if (s_pw_port == 0xFFFFFFFF) return;
    
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x1000; 
    msg.reply_port = 0xFFFFFFFF;
    syscall3(513, s_pw_port, (long)&msg, 0);
}

void leandros_audio_set_params(int freq, int channels) {
    if (s_pw_port == 0xFFFFFFFF) return;
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x100; 
    msg.reply_port = 0xFFFFFFFF;
    msg.data[0] = (uint8_t)(freq & 0xFF);
    msg.data[1] = (uint8_t)((freq >> 8) & 0xFF);
    msg.data[2] = (uint8_t)((freq >> 16) & 0xFF);
    msg.data[3] = (uint8_t)((freq >> 24) & 0xFF);
    msg.data[4] = (uint8_t)channels;
    syscall3(513, s_pw_port, (long)&msg, 0);
}

// ── SDL 3 Native Audio Stubs ────────────────────────────────────────────────

int SDL_Init(uint32_t flags) {
    if (flags & 0x00000010) leandros_audio_init();
    return 0;
}
int SDL_InitSubSystem(uint32_t flags) { return SDL_Init(flags); }
void SDL_QuitSubSystem(uint32_t flags) {}

// ── SDL_mixer Stubs ─────────────────────────────────────────────────────────

int Mix_OpenAudio(int frequency, uint16_t format, int channels, int chunksize) {
    leandros_audio_set_params(frequency, channels);
    return 0;
}
int Mix_AllocateChannels(int numchans) { return numchans; }
int Mix_Volume(int channel, int volume) { return volume; }
int Mix_PlayChannelTimed(int channel, Mix_Chunk *chunk, int loops, int ticks) {
    if (channel < 0 || channel >= MIX_CHANNELS) channel = 0;
    if (chunk && chunk->abuf) {
        s_channels[channel].data = chunk->abuf;
        s_channels[channel].len = chunk->alen;
        s_channels[channel].pos = 0;
        s_channels[channel].active = 1;
    }
    return channel;
}
void Mix_HaltChannel(int channel) { if (channel >= 0 && channel < MIX_CHANNELS) s_channels[channel].active = 0; }
int Mix_Playing(int channel) { if (channel >= 0 && channel < MIX_CHANNELS) return s_channels[channel].active; return 0; }
void Mix_CloseAudio(void) {}
int Mix_QuerySpec(int *frequency, uint16_t *format, int *channels) {
    if (frequency) *frequency = 44100;
    if (format) *format = 0x8010;
    if (channels) *channels = 2;
    return 1;
}
const SDL_version *Mix_Linked_Version(void) { static SDL_version v = {1, 2, 8}; return &v; }
const char *Mix_GetError(void) { return "No error"; }
int Mix_SetPanning(int channel, uint8_t left, uint8_t right) { return 1; }
int Mix_UnregisterAllEffects(int channel) { return 1; }
int Mix_HaltMusic(void) { s_seq.active = 0; return 0; }
int Mix_VolumeMusic(int volume) { return volume; }
int Mix_PlayMusic(Mix_Music *music, int loops) {
    if (!music) return 0;
    s_seq.music = music;
    s_seq.ptr = 14; 
    s_seq.active = 1;
    s_seq.samples_left_in_delta = 0;
    s_seq.tempo = 500000;
    s_seq.division = (music->data[12] << 8) | music->data[13];
    return 0;
}
void Mix_FreeMusic(Mix_Music *music) { if (music) { if (music->data) free(music->data); free(music); } }
Mix_Music *Mix_LoadMUS(const char *file) {
    char log[128];
    int n = snprintf(log, sizeof(log), "[SDL] Mix_LoadMUS: %s\n", file);
    write(1, log, n);

    int fd = open(file, O_RDONLY, 0);
    if (fd < 0) {
        write(1, "[SDL]   Failed to open MUS file\n", 31);
        return NULL;
    }
    uint32_t size = lseek(fd, 0, SEEK_END);
    lseek(fd, 0, SEEK_SET);
    
    if (size == 0) {
        write(1, "[SDL]   File is empty\n", 22);
        close(fd);
        return NULL;
    }

    Mix_Music* m = malloc(sizeof(Mix_Music));
    m->data = malloc(size);
    m->len = size;
    read(fd, m->data, size);
    close(fd);
    
    n = snprintf(log, sizeof(log), "[SDL]   Loaded %d bytes\n", (int)size);
    write(1, log, n);

    return m;
}
int Mix_PlayingMusic(void) { return s_seq.active; }
void Mix_SetMusicCMD(const char *command) {}
int Mix_RegisterEffect(int chan, Mix_EffectFunc_t f, Mix_EffectDone_t d, void *arg) { return 0; }
int Mix_SetMusicPosition(double position) { return 0; }
void SDL_PauseAudio(int pause_on) {}
void SDL_LockAudio(void) {}
void SDL_UnlockAudio(void) {}

// ── Graphics Implementation ──────────────────────────────────────────────────

struct drm_framebuffer {
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t format;
    void* buffer;
    uint32_t fb_id;
};

#define DRM_IOCTL_SET_MODE      0x1001
#define DRM_IOCTL_CREATE_FB     0x1002
#define DRM_IOCTL_GET_MODE      0x1003
#define DRM_IOCTL_FLIP_PAGE     0x1004
#define DRM_IOCTL_GET_CAPS      0x1006
#define DRM_CAP_ASYNC_PAGE_FLIP 0x7

static int s_drm_fd = -1;
static struct drm_framebuffer s_fb_primary;
static struct drm_framebuffer s_fb_back;
static int s_double_buffered = 0;
static uint32_t s_screen_width, s_screen_height;

static int drm_create_fb(struct drm_framebuffer* fb, uint32_t w, uint32_t h) {
    fb->width = w;
    fb->height = h;
    fb->format = 0x34325258; // XRGB8888
    fb->pitch = w * 4;

    uint32_t create_data[6] = { w, h, fb->format, 0, 0, 0 };
    if (ioctl(s_drm_fd, DRM_IOCTL_CREATE_FB, (unsigned long)create_data) != 0) return -1;

    fb->fb_id = create_data[3];
    fb->buffer = NULL;
    uint64_t mmap_offset = ((uint64_t)create_data[5] << 32) | create_data[4];

    void* mapped = mmap(NULL, w * h * 4, 3, 1, s_drm_fd, (long)mmap_offset);
    if (mapped != (void*)-1) fb->buffer = mapped;
    else return -1;
    
    return 0;
}

SDL_Window* SDL_CreateWindow(const char* title, int x, int y, int w, int h, uint32_t flags) {
    s_drm_fd = open("/dev/dri/card0", O_RDWR, 0);
    
    if (s_drm_fd >= 0) {
        uint32_t mode_data[3];
        if (ioctl(s_drm_fd, DRM_IOCTL_GET_MODE, (unsigned long)mode_data) == 0) {
            s_screen_width = mode_data[0];
            s_screen_height = mode_data[1];
            
            // Take ownership and disable console
            ioctl(s_drm_fd, DRM_IOCTL_SET_MODE, (unsigned long)mode_data);

            if (drm_create_fb(&s_fb_primary, DOOMGENERIC_RESX, DOOMGENERIC_RESY) == 0) {
                // Initial flip to start scanout
                uint32_t flip_data[4] = { s_fb_primary.fb_id, 0, DOOMGENERIC_RESX, DOOMGENERIC_RESY };
                ioctl(s_drm_fd, DRM_IOCTL_FLIP_PAGE, (unsigned long)flip_data);

                uint32_t caps[2] = { DRM_CAP_ASYNC_PAGE_FLIP, 0 };
                if (ioctl(s_drm_fd, DRM_IOCTL_GET_CAPS, (unsigned long)caps) == 0 && caps[1]) {
                    if (drm_create_fb(&s_fb_back, DOOMGENERIC_RESX, DOOMGENERIC_RESY) == 0) {
                        s_double_buffered = 1;
                        write(1, "[SDL] DRM double buffering active\n", 34);
                    }
                }
                return (SDL_Window*)0x1234;
            }
        }
    }
    
    // Fallback
    if (s_drm_fd < 0) s_drm_fd = open("/dev/fb0", O_RDWR, 0);
    if (s_drm_fd >= 0) {
        uint32_t info[8];
        if (ioctl(s_drm_fd, 0x4600, (unsigned long)info) == 0) {
            s_screen_width = info[0]; s_screen_height = info[1];
        } else {
            s_screen_width = 320; s_screen_height = 200;
        }
        write(1, "[SDL] Using legacy FB rendering\n", 32);
    }
    return (SDL_Window*)0x1234;
}

SDL_Renderer* SDL_CreateRenderer(SDL_Window* window, int index, uint32_t flags) { return (SDL_Renderer*)0x5678; }
SDL_Texture* SDL_CreateTexture(SDL_Renderer* renderer, uint32_t format, int access, int w, int h) { return (SDL_Texture*)0x9ABC; }

void SDL_UpdateTexture(SDL_Texture* texture, const void* rect, const void* pixels, int pitch) {
    leandros_audio_pump();
    if (s_drm_fd < 0 || !pixels) return;

    if (s_fb_primary.buffer) {
        // High performance DRM path
        void* dest = s_double_buffered ? s_fb_back.buffer : s_fb_primary.buffer;
        memcpy(dest, pixels, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);

        if (s_double_buffered) {
            uint32_t flip_data[4] = { s_fb_back.fb_id, 0, DOOMGENERIC_RESX, DOOMGENERIC_RESY };
            ioctl(s_drm_fd, DRM_IOCTL_FLIP_PAGE, (unsigned long)flip_data);
            
            // Swap
            struct drm_framebuffer tmp = s_fb_primary;
            s_fb_primary = s_fb_back;
            s_fb_back = tmp;
        }
    } else {
        // Legacy FB path
        if (s_screen_width == DOOMGENERIC_RESX && s_screen_height == DOOMGENERIC_RESY) {
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
                if (x_map) for (int x = 0; x < s_screen_width; x++) x_map[x] = x * DOOMGENERIC_RESX / s_screen_width;
                last_width = s_screen_width; last_height = s_screen_height;
            }
            if (!scaled_buffer || !x_map) return;
            const uint32_t* src_pixels = (const uint32_t*)pixels;
            for (int y = 0; y < s_screen_height; y++) {
                int src_y = y * DOOMGENERIC_RESY / s_screen_height;
                const uint32_t* src_row = &src_pixels[src_y * DOOMGENERIC_RESX];
                uint32_t* dest_row = &scaled_buffer[y * s_screen_width];
                for (int x = 0; x < s_screen_width; x++) dest_row[x] = src_row[x_map[x]];
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
    if (ev_fd == -2) ev_fd = open("/dev/input/event0", O_RDONLY, 0); 
    if (ev_fd < 0) return 0;
    int bytes_available = 0;
    if (ioctl(ev_fd, FIONREAD, &bytes_available) < 0 || bytes_available < 1) return 0;
    struct { uint64_t sec, usec; uint16_t type, code; int32_t value; } ev;
    if (read(ev_fd, &ev, sizeof(ev)) == sizeof(ev)) {
        if (ev.type == 1) {
            event->type = (ev.value == 0) ? SDL_KEYUP : SDL_KEYDOWN;
            uint32_t sym = 0;
            if (ev.value == 2) { // Auto-repeat or Serial ASCII input
                if (ev.code >= 'a' && ev.code <= 'z') sym = ev.code;
                else if (ev.code >= 'A' && ev.code <= 'Z') sym = ev.code - 'A' + 'a';
                else if (ev.code >= '0' && ev.code <= '9') sym = ev.code;
                else {
                    switch (ev.code) {
                        case '\r':
                        case '\n': sym = SDLK_RETURN; break;
                        case 0x1B: sym = SDLK_ESCAPE; break;
                        case '\t': sym = SDLK_TAB; break;
                        case 0x08:
                        case 0x7F: sym = SDLK_BACKSPACE; break;
                        case ' ': sym = SDLK_SPACE; break;
                        case 21: sym = SDLK_y; break; // Some drivers send scancode 21 with value 2
                        case 49: sym = SDLK_n; break; // Some drivers send scancode 49 with value 2
                        default: sym = 0; break;
                    }
                }
            } else { // Keyboard scancode input (ev.value 0 or 1)
                switch (ev.code) {
                    case 1: sym = SDLK_ESCAPE; break;
                    case 2: sym = SDLK_1; break;
                    case 3: sym = SDLK_2; break;
                    case 4: sym = SDLK_3; break;
                    case 5: sym = SDLK_4; break;
                    case 6: sym = SDLK_5; break;
                    case 7: sym = SDLK_6; break;
                    case 8: sym = SDLK_7; break;
                    case 9: sym = SDLK_8; break;
                    case 10: sym = SDLK_9; break;
                    case 11: sym = SDLK_0; break;
                    case 12: sym = SDLK_MINUS; break;
                    case 13: sym = SDLK_EQUALS; break;
                    case 14: sym = SDLK_BACKSPACE; break;
                    case 15: sym = SDLK_TAB; break;
                    case 16: sym = SDLK_q; break;
                    case 17: sym = SDLK_w; break;
                    case 18: sym = SDLK_e; break;
                    case 19: sym = SDLK_r; break;
                    case 20: sym = SDLK_t; break;
                    case 21: sym = SDLK_y; break;
                    case 22: sym = SDLK_u; break;
                    case 23: sym = SDLK_i; break;
                    case 24: sym = SDLK_o; break;
                    case 25: sym = SDLK_p; break;
                    case 28: sym = SDLK_RETURN; break;
                    case 29: sym = SDLK_LCTRL; break;
                    case 30: sym = SDLK_a; break;
                    case 31: sym = SDLK_s; break;
                    case 32: sym = SDLK_d; break;
                    case 33: sym = SDLK_f; break;
                    case 34: sym = SDLK_g; break;
                    case 35: sym = SDLK_h; break;
                    case 36: sym = SDLK_j; break;
                    case 37: sym = SDLK_k; break;
                    case 38: sym = SDLK_l; break;
                    case 42: sym = SDLK_LSHIFT; break;
                    case 44: sym = SDLK_z; break;
                    case 45: sym = SDLK_x; break;
                    case 46: sym = SDLK_c; break;
                    case 47: sym = SDLK_v; break;
                    case 48: sym = SDLK_b; break;
                    case 49: sym = SDLK_n; break;
                    case 50: sym = SDLK_m; break;
                    case 54: sym = SDLK_RSHIFT; break;
                    case 56: sym = SDLK_LALT; break;
                    case 57: sym = SDLK_SPACE; break;
                    case 97: sym = SDLK_RCTRL; break;
                    case 100: sym = SDLK_RALT; break;
                    case 103: sym = SDLK_UP; break;
                    case 108: sym = SDLK_DOWN; break;
                    case 105: sym = SDLK_LEFT; break;
                    case 106: sym = SDLK_RIGHT; break;
                    default: sym = 0; break;
                }
            }
            if (sym == 0) return 0;
            event->key.keysym.sym = sym;
            return 1;
        }
    }
    return 0;
}
uint32_t SDL_GetTicks(void) { struct timespec ts; clock_gettime(1, &ts); return (ts.tv_sec * 1000) + (ts.tv_nsec / 1000000); }
void SDL_Delay(uint32_t ms) { usleep(ms * 1000); }
void SDL_SetWindowTitle(SDL_Window* window, const char* title) {}
void SDL_Quit(void) { if (s_drm_fd >= 0) close(s_drm_fd); s_drm_fd = -1; }
