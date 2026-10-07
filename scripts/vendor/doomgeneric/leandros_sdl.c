// Vendored copy: the source of truth is leandros/scripts/vendor/doomgeneric/
// leandros_sdl.c; build-all.sh copies it into the shared ../doomgeneric
// checkout. Edit it there, not here.
#include <SDL.h>
#include <unistd.h>
#include <stdio.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <SDL_mixer.h>
#include "doomgeneric.h"
#include "leandros_midi.h"
#include "leandros_wayland.h"

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

// ── Mixer State ────────────────────────────────────────────────────────────

#define MIX_CHANNELS 16
#define SAMPLE_RATE 44100

typedef struct {
    const int16_t *data;
    uint32_t frames, pos;
    int active, volume;
    uint8_t left, right;
} mixer_channel_t;

static mixer_channel_t s_channels[MIX_CHANNELS];
static uint32_t s_last_pump_ticks;
static uint32_t s_pump_remainder;
static int s_audio_open;
static Mix_EffectFunc_t s_post_effect;
static void *s_post_arg;

// ── Audio Core ──────────────────────────────────────────────────────────────

// ── PipeWire output (pw-cat) ────────────────────────────────────────────────
//
// The kernel audio device behind the legacy IPC port is single-writer, like an
// ALSA hw PCM without dmix (servers/pipewire, commit 24bf6e8). In a desktop
// session PipeWire's leandros-snd-sink holds it for the whole session, so the
// legacy path is refused EBUSY there -- in a terminal inside the desktop AND on
// a text VT while the desktop runs -- and DOOM ignored the refusal and played
// into nothing. A program in a PipeWire session plays through PipeWire. DOOM
// has no libpipewire (static LeandrOS libc), so it hands its mixed PCM to
// PipeWire's own player, pw-cat, over a pipe: a real PipeWire stream, mixed
// with everything else the session plays.

extern int fork(void);
extern int execve(const char *path, char *const argv[], char *const envp[]);
extern int dup3(int oldfd, int newfd, int flags);
extern int pipe2(int pipefd[2], int flags);

#ifdef __aarch64__
#define A_NR_FACCESSAT 48
#define A_NR_FCNTL 25
#define A_NR_RT_SIGACTION 134
#define A_NR_EXIT_GROUP 94
#else
#define A_NR_FACCESSAT 269
#define A_NR_FCNTL 72
#define A_NR_RT_SIGACTION 13
#define A_NR_EXIT_GROUP 231
#endif

static int s_pcm_pipe = -1;        // write end of the pipe into pw-cat, or -1
static uint64_t s_pcm_written;     // bytes accepted by the pipe (frame alignment)
static uint64_t s_pcm_dropped;     // bytes dropped because the pipe was full
static void audio_pipe_write(const uint8_t *p, size_t len);

// Start pw-cat on this session's PipeWire. 1 = PCM now goes there.
static int audio_open_pipewire(void) {
    const char *dir = wl_getenv("XDG_RUNTIME_DIR");
    if (!dir || !*dir) return 0;
    char sock[200];
    size_t dl = strlen(dir);
    if (dl + 12 >= sizeof sock) return 0;
    memcpy(sock, dir, dl);
    memcpy(sock + dl, "/pipewire-0", 12);
    if (wl_sys(A_NR_FACCESSAT, -100 /* AT_FDCWD */, (long)sock, 0 /* F_OK */, 0, 0) != 0)
        return 0;
    if (wl_sys(A_NR_FACCESSAT, -100, (long)"/usr/bin/pw-cat", 1 /* X_OK */, 0, 0) != 0) {
        wl_say("PipeWire is running (", sock, ") but /usr/bin/pw-cat is missing: no sound");
        return 0;
    }

    int fds[2];
    if (pipe2(fds, 02000000 /* O_CLOEXEC */) != 0) { wl_say("pipe2 failed: no sound", 0, 0); return 0; }
    int pid = fork();
    if (pid < 0) { close(fds[0]); close(fds[1]); wl_say("fork failed: no sound", 0, 0); return 0; }
    if (pid == 0) {
        dup3(fds[0], 0, 0);          // stdin = the pipe (dup3 clears CLOEXEC)
        char *argv[] = {
            "pw-cat", "--playback", "--media-role", "Game", "--latency", "40ms",
            "-P", "{ application.name = DOOM node.name = doom node.description = DOOM }",
            "-", 0
        };
        execve("/usr/bin/pw-cat", argv, wl_envp());
        wl_sys(A_NR_EXIT_GROUP, 127, 0, 0, 0, 0);
    }
    close(fds[0]);
    // Never let a stalled player stall the game: the write end is non-blocking
    // and a full pipe drops audio rather than frames.
    long fl = wl_sys(A_NR_FCNTL, fds[1], 3 /* F_GETFL */, 0, 0, 0);
    if (fl >= 0) wl_sys(A_NR_FCNTL, fds[1], 4 /* F_SETFL */, fl | 04000 /* O_NONBLOCK */, 0, 0);
    // pw-cat exiting must cost DOOM its sound, not its life (SIGPIPE).
    struct { unsigned long handler, flags, restorer, mask; } ign = { 1 /* SIG_IGN */, 0, 0, 0 };
    wl_sys(A_NR_RT_SIGACTION, 13 /* SIGPIPE */, (long)&ign, 0, 8, 0);
    s_pcm_pipe = fds[1];
    // This pw-cat has no raw mode: it reads its input through libsndfile, so
    // the stream opens with a WAV header (44.1 kHz, stereo, S16LE) whose RIFF
    // and data sizes are 0xFFFFFFFF, "until the stream ends", which
    // libsndfile accepts from a pipe. Measured in the guest: pw-cat plays it.
    static const uint8_t wavhdr[44] = {
        'R','I','F','F', 0xff,0xff,0xff,0xff, 'W','A','V','E',
        'f','m','t',' ', 16,0,0,0, 1,0, 2,0, 0x44,0xac,0,0, 0x10,0xb1,0x02,0, 4,0, 16,0,
        'd','a','t','a', 0xff,0xff,0xff,0xff
    };
    audio_pipe_write(wavhdr, sizeof wavhdr);
    if (s_pcm_pipe < 0) return 0;
    write(1, "[doom] sound: PipeWire stream via pw-cat\n", 41);
    return 1;
}

static void audio_pipe_write(const uint8_t *p, size_t len) {
    // Keep the stream frame-aligned (4 bytes) across short writes: pad the
    // previous partial frame with silence before anything new.
    static const uint8_t zero[4];
    unsigned mis = (unsigned)(s_pcm_written & 3);
    if (mis) {
        long r = write(s_pcm_pipe, zero, 4 - mis);
        if (r > 0) s_pcm_written += (uint64_t)r;
        if ((s_pcm_written & 3) != 0) { s_pcm_dropped += len; return; }
    }
    while (len) {
        long r = write(s_pcm_pipe, p, len);
        if (r > 0) { s_pcm_written += (uint64_t)r; p += r; len -= (size_t)r; continue; }
        if (r < 0 && errno == 32 /* EPIPE */) {
            wl_say("pw-cat exited: no more sound", 0, 0);
            close(s_pcm_pipe);
            s_pcm_pipe = -1;
            s_audio_open = 0;
            return;
        }
        s_pcm_dropped += len;        // EAGAIN: the player is behind, drop
        return;
    }
}

void leandros_audio_send_pcm(const void* data, size_t len) {
    if (s_pcm_pipe >= 0) { audio_pipe_write((const uint8_t*)data, len); return; }
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

void leandros_audio_pump(void) {
    if (!s_audio_open || (s_pcm_pipe < 0 && s_pw_port == 0xFFFFFFFF)) return;
    uint32_t now = SDL_GetTicks();
    uint32_t dt_ms = now - s_last_pump_ticks;
    if (dt_ms < 10) return;
    s_last_pump_ticks = now;
    // Bound catch-up after stalls; retain fractional frames during normal play.
    if (dt_ms > 100) dt_ms = 100;
    uint32_t scaled = SAMPLE_RATE * dt_ms + s_pump_remainder;
    uint32_t remaining = scaled / 1000;
    s_pump_remainder = scaled % 1000;
    float music_buf[512 * 2];
    int16_t stereo_buf[512 * 2];
    while (remaining) {
        uint32_t frames = remaining > 512 ? 512 : remaining;
        GM_Render(music_buf, frames);
        for (uint32_t frame = 0; frame < frames; ++frame) {
            float left = music_buf[frame * 2] * 32767.0f;
            float right = music_buf[frame * 2 + 1] * 32767.0f;
            for (int c = 0; c < MIX_CHANNELS; ++c) {
                mixer_channel_t *channel = &s_channels[c];
                if (!channel->active) continue;
                float gain = channel->volume / (128.0f * 255.0f);
                left += channel->data[channel->pos * 2] * gain * channel->left;
                right += channel->data[channel->pos * 2 + 1] * gain * channel->right;
                if (++channel->pos >= channel->frames) channel->active = 0;
            }
            if (left > 32767) left = 32767;
            if (left < -32768) left = -32768;
            if (right > 32767) right = 32767;
            if (right < -32768) right = -32768;
            stereo_buf[frame * 2] = (int16_t)left;
            stereo_buf[frame * 2 + 1] = (int16_t)right;
        }
        if (s_post_effect) s_post_effect(MIX_CHANNEL_POST, stereo_buf, frames * 4, s_post_arg);
        leandros_audio_send_pcm(stereo_buf, frames * 4);
        remaining -= frames;
    }
    if (s_pcm_pipe >= 0) return;
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

long leandros_audio_set_params(int freq, int channels) {
    if (s_pw_port == 0xFFFFFFFF) return -19;
    struct ipc_msg msg __attribute__((aligned(8)));
    memset(&msg, 0, sizeof(msg));
    msg.tag = 0x100; 
    msg.reply_port = 0xFFFFFFFF;
    msg.data[0] = (uint8_t)(freq & 0xFF);
    msg.data[1] = (uint8_t)((freq >> 8) & 0xFF);
    msg.data[2] = (uint8_t)((freq >> 16) & 0xFF);
    msg.data[3] = (uint8_t)((freq >> 24) & 0xFF);
    msg.data[4] = (uint8_t)channels;
    long rc = syscall3(513, s_pw_port, (long)&msg, 0);
    if (rc < 0) return rc;
    int64_t reply;
    memcpy(&reply, msg.data, 8);    // the server's answer, in place
    return reply < 0 ? (long)reply : 0;
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
    if (format != AUDIO_S16SYS || channels != 2) return -1;
    // Both outputs consume 44.1 kHz stereo. QuerySpec tells Doom the actual
    // rate so its cached sound effects are resampled consistently.
    if (s_pcm_pipe < 0 && !audio_open_pipewire()) {
        long rc = leandros_audio_set_params(SAMPLE_RATE, 2);
        if (rc == -16) {
            wl_say("sound: the audio device is busy (held by a PipeWire session or another player) "
                   "and this session's PipeWire is not reachable (XDG_RUNTIME_DIR/pipewire-0); no sound", 0, 0);
            return -1;
        }
        if (rc == -13) {
            wl_say("sound: permission denied on the audio device (not in the audio group); no sound", 0, 0);
            return -1;
        }
        if (rc < 0) {
            wl_say_errno("sound: the audio device refused SET_PARAMS; no sound", rc);
            return -1;
        }
        write(1, "[doom] sound: audio device (direct)\n", 36);
    }
    s_audio_open = 1;
    s_last_pump_ticks = SDL_GetTicks();
    s_pump_remainder = 0;
    for (int i = 0; i < MIX_CHANNELS; ++i) {
        s_channels[i].volume = MIX_MAX_VOLUME;
        s_channels[i].left = s_channels[i].right = 255;
    }
    return 0;
}
int Mix_AllocateChannels(int numchans) { return numchans > MIX_CHANNELS ? MIX_CHANNELS : numchans; }
int Mix_Volume(int channel, int volume) {
    int previous = 0;
    if (volume > MIX_MAX_VOLUME) volume = MIX_MAX_VOLUME;
    for (int i = 0; i < MIX_CHANNELS; ++i) if (channel == -1 || channel == i) {
        previous = s_channels[i].volume;
        if (volume >= 0) s_channels[i].volume = volume;
    }
    return previous;
}
int Mix_PlayChannelTimed(int channel, Mix_Chunk *chunk, int loops, int ticks) {
    if (channel < 0) {
        for (channel = 0; channel < MIX_CHANNELS && s_channels[channel].active; ++channel) {}
    }
    if (channel >= MIX_CHANNELS || !chunk || !chunk->abuf || chunk->alen < 4) return -1;
    s_channels[channel].data = (const int16_t *)chunk->abuf;
    s_channels[channel].frames = chunk->alen / 4;
    s_channels[channel].pos = 0;
    s_channels[channel].active = 1;
    return channel;
}
void Mix_HaltChannel(int channel) {
    for (int i = 0; i < MIX_CHANNELS; ++i)
        if (channel == -1 || channel == i) s_channels[i].active = 0;
}
int Mix_Playing(int channel) { return channel >= 0 && channel < MIX_CHANNELS ? s_channels[channel].active : 0; }
void Mix_CloseAudio(void) {
    if (s_pcm_pipe >= 0) { close(s_pcm_pipe); s_pcm_pipe = -1; }
    GM_Shutdown();
    Mix_HaltChannel(-1);
    s_audio_open = 0;
    s_post_effect = NULL;
    s_post_arg = NULL;
}
int Mix_QuerySpec(int *frequency, uint16_t *format, int *channels) {
    if (frequency) *frequency = SAMPLE_RATE;
    if (format) *format = AUDIO_S16SYS;
    if (channels) *channels = 2;
    return s_audio_open;
}
const SDL_version *Mix_Linked_Version(void) { static SDL_version v = {1, 2, 12}; return &v; }
const char *Mix_GetError(void) { return GM_Error(); }
int Mix_SetPanning(int channel, uint8_t left, uint8_t right) {
    if (channel < 0 || channel >= MIX_CHANNELS) return 0;
    s_channels[channel].left = left;
    s_channels[channel].right = right;
    return 1;
}
int Mix_UnregisterAllEffects(int channel) {
    if (channel == MIX_CHANNEL_POST) s_post_effect = NULL;
    if (channel >= 0 && channel < MIX_CHANNELS)
        s_channels[channel].left = s_channels[channel].right = 255;
    return 1;
}
int Mix_InitMIDI(void) { return GM_Init(SAMPLE_RATE); }
void Mix_PauseMusic(void) { GM_Pause(1); }
void Mix_ResumeMusic(void) { GM_Pause(0); }
int Mix_HaltMusic(void) { GM_Stop(); return 0; }
int Mix_VolumeMusic(int volume) { return GM_Volume(volume); }
int Mix_PlayMusic(Mix_Music *music, int loops) { return GM_Play((gm_music *)music, loops); }
void Mix_FreeMusic(Mix_Music *music) { GM_FreeMusic((gm_music *)music); }
Mix_Music *Mix_LoadMUS_Memory(const void *data, size_t length) {
    return (Mix_Music *)GM_LoadMIDI(data, length);
}
Mix_Music *Mix_LoadMUS(const char *file) {
    // External music commands and compressed substitute packs are unsupported;
    // a standard MIDI file can still be loaded as a substitute.
    int fd = open(file, O_RDONLY, 0);
    if (fd < 0) return NULL;
    long length = lseek(fd, 0, SEEK_END);
    if (length <= 0 || length > 4 * 1024 * 1024 || lseek(fd, 0, SEEK_SET) < 0) {
        close(fd); return NULL;
    }
    void *data = malloc((size_t)length);
    Mix_Music *music = NULL;
    size_t done = 0;
    while (data && done < (size_t)length) {
        long result = read(fd, (unsigned char *)data + done, length - done);
        if (result <= 0) break;
        done += result;
    }
    if (done == (size_t)length) music = Mix_LoadMUS_Memory(data, length);
    free(data);
    close(fd);
    return music;
}
int Mix_PlayingMusic(void) { return GM_Playing(); }
void Mix_SetMusicCMD(const char *command) {}
int Mix_RegisterEffect(int chan, Mix_EffectFunc_t f, Mix_EffectDone_t d, void *arg) {
    if (chan != MIX_CHANNEL_POST) return 0;
    s_post_effect = f;
    s_post_arg = arg;
    return 1;
}
int Mix_SetMusicPosition(double position) { return -1; }
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
    uint32_t mmap_offset = create_data[5];

    void* mapped = mmap(NULL, w * h * 4, 3, 1, s_drm_fd, (long)mmap_offset);
    if (mapped != (void*)-1) fb->buffer = mapped;
    else return -1;
    
    return 0;
}

SDL_Window* SDL_CreateWindow(const char* title, int x, int y, int w, int h, uint32_t flags) {
    // Under a compositor the display hardware is the compositor's: card0's
    // present ioctls are refused to a non-master and fb0 is not what it scans
    // out. Be a window on it instead (see leandros_wayland.h).
    int wr = wl_open();
    if (wr > 0) return (SDL_Window*)0x1234;
    if (wr < 0) {
        // A compositor is configured for this session and its display could
        // not be used. Falling back to card0 would make DOOM a KMS client on
        // a display the compositor owns: refused on its VT, and implicitly
        // granted on whichever text VT the user switches to next, where it
        // would paint over that console. Say so and stop instead.
        wl_say("not falling back to the display hardware while a compositor owns it; exiting", 0, 0);
        exit(1);
    }

    s_drm_fd = open("/dev/dri/card0", O_RDWR, 0);
    
    if (s_drm_fd >= 0) {
        uint32_t mode_data[3];
        if (ioctl(s_drm_fd, DRM_IOCTL_GET_MODE, (unsigned long)mode_data) == 0) {
            s_screen_width = mode_data[0];
            s_screen_height = mode_data[1];
            
            // Take ownership and disable console. EACCES means another
            // client (a compositor) is DRM master on the VT on screen: the
            // display is not ours, and presenting anyway would only make the
            // kernel grant DOOM the next VT the user switches to.
            if (ioctl(s_drm_fd, DRM_IOCTL_SET_MODE, (unsigned long)mode_data) != 0 && errno == 13) {
                wl_say("the display is owned by another program (DRM master, EACCES). "
                       "Run DOOM in a terminal inside the desktop, or on a text VT", 0, 0);
                exit(1);
            }

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
    
    // Fallback: the framebuffer device. card0 itself is no fallback — it is
    // still open here when GET_MODE or CREATE_FB failed (CREATE_FB refuses a
    // buffer above 4 GiB, which a 4 GiB guest hands out), and the legacy path
    // below would then write() frames into the DRM node instead of fb0.
    if (s_drm_fd >= 0) { close(s_drm_fd); s_drm_fd = -1; }
    s_drm_fd = open("/dev/fb0", O_RDWR, 0);
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
    if (wl.active) { if (pixels) wl_present((const uint32_t*)pixels); return; }
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
// evdev key code -> SDL keysym. Shared by the evdev reader and the Wayland
// keyboard, whose key events carry the same evdev codes.
static uint32_t leandros_evdev_to_sdlk(uint32_t code) {
    switch (code) {
        case 1: return SDLK_ESCAPE;
        case 2: return SDLK_1;
        case 3: return SDLK_2;
        case 4: return SDLK_3;
        case 5: return SDLK_4;
        case 6: return SDLK_5;
        case 7: return SDLK_6;
        case 8: return SDLK_7;
        case 9: return SDLK_8;
        case 10: return SDLK_9;
        case 11: return SDLK_0;
        case 12: return SDLK_MINUS;
        case 13: return SDLK_EQUALS;
        case 14: return SDLK_BACKSPACE;
        case 15: return SDLK_TAB;
        case 16: return SDLK_q;
        case 17: return SDLK_w;
        case 18: return SDLK_e;
        case 19: return SDLK_r;
        case 20: return SDLK_t;
        case 21: return SDLK_y;
        case 22: return SDLK_u;
        case 23: return SDLK_i;
        case 24: return SDLK_o;
        case 25: return SDLK_p;
        case 28: return SDLK_RETURN;
        case 29: return SDLK_LCTRL;
        case 30: return SDLK_a;
        case 31: return SDLK_s;
        case 32: return SDLK_d;
        case 33: return SDLK_f;
        case 34: return SDLK_g;
        case 35: return SDLK_h;
        case 36: return SDLK_j;
        case 37: return SDLK_k;
        case 38: return SDLK_l;
        case 42: return SDLK_LSHIFT;
        case 44: return SDLK_z;
        case 45: return SDLK_x;
        case 46: return SDLK_c;
        case 47: return SDLK_v;
        case 48: return SDLK_b;
        case 49: return SDLK_n;
        case 50: return SDLK_m;
        case 54: return SDLK_RSHIFT;
        case 56: return SDLK_LALT;
        case 57: return SDLK_SPACE;
        case 97: return SDLK_RCTRL;
        case 100: return SDLK_RALT;
        case 103: return SDLK_UP;
        case 108: return SDLK_DOWN;
        case 105: return SDLK_LEFT;
        case 106: return SDLK_RIGHT;
        default: return 0;
    }
}

int SDL_PollEvent(SDL_Event* event) {
    leandros_audio_pump();
    if (wl.active) {
        // Input comes from the window's wl_keyboard, i.e. only while DOOM has
        // keyboard focus, never from the raw evdev node the compositor reads.
        for (;;) {
            int k = wl_next_key();
            if (k == -2) { event->type = SDL_QUIT; return 1; }
            if (k < 0) return 0;
            uint32_t sym = leandros_evdev_to_sdlk((uint32_t)k & 0xffff);
            if (!sym) continue;
            event->type = (k >> 16) ? SDL_KEYDOWN : SDL_KEYUP;
            event->key.keysym.sym = sym;
            return 1;
        }
    }
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
                sym = leandros_evdev_to_sdlk(ev.code);
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
void SDL_Quit(void) { wl_teardown(); if (s_drm_fd >= 0) close(s_drm_fd); s_drm_fd = -1; }
