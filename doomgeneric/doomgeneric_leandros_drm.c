//! DRM-based DOOM implementation for LeandrOS
//! Uses the DRM subsystem for hardware-accelerated rendering
//! Falls back to legacy framebuffer if DRM is unavailable

#include "doomgeneric.h"
#include "leandros_libc.h"
#include "doomkeys.h"

// Global state
static int use_drm = 0;  // 0 = legacy framebuffer, 1 = DRM

// DRM interface structures
struct drm_mode_info {
    uint32_t width;
    uint32_t height;
    uint32_t refresh_rate;
};

struct drm_framebuffer {
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t format;
    void* buffer;
    uint32_t fb_id;
};

// DRM device state
static int drm_fd = -1;
static int ev_fd = -1;
static struct drm_framebuffer primary_fb;
static struct drm_framebuffer back_fb;
static uint32_t screen_width, screen_height;
static uint32_t* scaling_buffer = NULL;
static int double_buffered = 0;
static int graphics_disabled = 0;

// DRM ioctl commands
#define DRM_IOCTL_SET_MODE      0x1001
#define DRM_IOCTL_CREATE_FB     0x1002
#define DRM_IOCTL_GET_MODE      0x1003
#define DRM_IOCTL_FLIP_PAGE     0x1004
#define DRM_IOCTL_SET_PLANE     0x1005
#define DRM_IOCTL_GET_CAPS      0x1006

// DRM capabilities
#define DRM_CAP_DUMB_BUFFER     0x1
#define DRM_CAP_VBLANK          0x2
#define DRM_CAP_PRIME           0x3
#define DRM_CAP_ASYNC_PAGE_FLIP 0x7
#define DRM_CAP_CURSOR_WIDTH    0x8
#define DRM_CAP_CURSOR_HEIGHT   0x9

static int drm_create_framebuffer(struct drm_framebuffer* fb, uint32_t width, uint32_t height) {
    if (drm_fd < 0) return -1;

    fb->width = width;
    fb->height = height;
    fb->format = 0x34325258; // DRM_FORMAT_XRGB8888
    fb->pitch = width * 4;

    // Request buffer creation via ioctl
    uint32_t create_data[6] = {
        width, height, fb->format, 0, 0, 0
    };

    if (ioctl(drm_fd, DRM_IOCTL_CREATE_FB, (unsigned long)create_data) != 0) {
        return -1;
    }

    fb->fb_id = create_data[3];
    fb->buffer = (void*)(unsigned long)create_data[4];
    uint32_t mmap_offset = create_data[5];

    printf("DOOM: DRM FB created (id=%d, size=%dx%d, offset=0x%x)\n", 
           fb->fb_id, width, height, mmap_offset);

    // Check if we got a direct memory buffer or need to use file operations
    if (fb->buffer == NULL) {
        printf("DOOM: DRM trying mmap for direct access...\n");
        // Attempt to map the device memory directly
        // PROT_READ | PROT_WRITE (3), MAP_SHARED (1)
        void* mapped = mmap(NULL, width * height * 4, 3, 1, drm_fd, (long)mmap_offset);
        if (mapped != (void*)-1) {
            fb->buffer = mapped;
            printf("DOOM: DRM successfully mmap'd framebuffer at %p\n", fb->buffer);
        } else {
            printf("DOOM: DRM mmap failed, using file-based framebuffer access\n");
        }
    } else {
        printf("DOOM: DRM using direct memory buffer at %p\n", fb->buffer);
    }

    return 0;
}

static int drm_set_mode(uint32_t width, uint32_t height, uint32_t refresh) {
    if (drm_fd < 0) return -1;

    uint32_t mode_data[3] = { width, height, refresh };
    return ioctl(drm_fd, DRM_IOCTL_SET_MODE, (unsigned long)mode_data);
}

static int drm_get_current_mode(struct drm_mode_info* mode) {
    if (drm_fd < 0) return -1;

    uint32_t mode_data[3];
    printf("DOOM: Calling DRM ioctl GET_MODE (cmd=0x%x, fd=%d)\n", DRM_IOCTL_GET_MODE, drm_fd);
    printf("DOOM: ioctl parameters: fd=%d, cmd=0x%x, arg=0x%lx\n", drm_fd, DRM_IOCTL_GET_MODE, (unsigned long)mode_data);
    int result = ioctl(drm_fd, DRM_IOCTL_GET_MODE, (unsigned long)mode_data);
    printf("DOOM: DRM ioctl returned %d (unsigned: %u)\n", result, (unsigned int)result);

    if (result == 0) {
        mode->width = mode_data[0];
        mode->height = mode_data[1];
        mode->refresh_rate = mode_data[2];
        printf("DOOM: Got mode %dx%d@%dHz from DRM\n", mode->width, mode->height, mode->refresh_rate);
        return 0;
    }
    printf("DOOM: DRM ioctl failed, falling back to legacy\n");
    return -1;
}

static int drm_flip_page() {
    if (drm_fd < 0 || !double_buffered) return -1;

    // Use hardware scaling: pass source framebuffer dimensions to DRM
    uint32_t flip_data[4] = {
        back_fb.fb_id,      // Framebuffer ID
        0,                  // Flags (async flip)
        DOOMGENERIC_RESX,   // Source width (320)
        DOOMGENERIC_RESY    // Source height (200)
    };

    return ioctl(drm_fd, DRM_IOCTL_FLIP_PAGE, (unsigned long)flip_data);
}

static int drm_get_capabilities() {
    if (drm_fd < 0) return 0;

    uint32_t caps[2];
    int capabilities = 0;

    // Check for double buffering capability
    caps[0] = DRM_CAP_ASYNC_PAGE_FLIP;
    if (ioctl(drm_fd, DRM_IOCTL_GET_CAPS, (unsigned long)caps) == 0 && caps[1]) {
        capabilities |= 1; // Double buffering supported
    }

    return capabilities;
}

void DG_Init() {
    ev_fd = open("/dev/input/event0", 0, 0); // O_RDONLY

    // Try to open DRM device first
    printf("DOOM: Attempting DRM initialization...\n");
    printf("DOOM: Opening device file: /dev/dri/card0\n");
    drm_fd = open("/dev/dri/card0", 2, 0); // O_RDWR
    printf("DOOM: open() returned fd = %d\n", drm_fd);

    if (drm_fd >= 0) {
        // Try DRM initialization
        struct drm_mode_info current_mode;
        if (drm_get_current_mode(&current_mode) == 0) {
            // DRM is available and working
            printf("DOOM: DRM mode enabled - hardware acceleration active\n");
            use_drm = 1;
            screen_width = current_mode.width;
            screen_height = current_mode.height;
            printf("DOOM: DRM display mode: %dx%d@%dHz\n",
                   current_mode.width, current_mode.height, current_mode.refresh_rate);

            // Create primary framebuffer at source resolution for hardware scaling
            if (drm_create_framebuffer(&primary_fb, DOOMGENERIC_RESX, DOOMGENERIC_RESY) == 0) {
                // Check for double buffering capability
                int caps = drm_get_capabilities();
                if (caps & 1) {
                    // Create back buffer for double buffering at source resolution
                    if (drm_create_framebuffer(&back_fb, DOOMGENERIC_RESX, DOOMGENERIC_RESY) == 0) {
                        double_buffered = 1;
                        printf("DOOM: DRM double buffering enabled\n");
                    }
                }

                return; // Success with DRM
            }
        }

        // DRM failed, close and fall back to framebuffer
        printf("DOOM: DRM initialization failed, falling back to legacy mode\n");
        close(drm_fd);
        drm_fd = -1;
    } else {
        printf("DOOM: DRM device not available, using legacy framebuffer\n");
    }

    // Fallback to legacy framebuffer
    use_drm = 0;
    printf("DOOM: Initializing legacy framebuffer mode\n");
    drm_fd = open("/dev/fb0", 1, 0); // O_WRONLY
    if (drm_fd < 0) {
        // No framebuffer available - set defaults but continue
        printf("DOOM: Warning - No graphics device available\n");
        screen_width = DOOMGENERIC_RESX;
        screen_height = DOOMGENERIC_RESY;
        return;
    }

    // Legacy framebuffer mode
    printf("DOOM: Legacy framebuffer mode active\n");
    uint32_t info[8];
    if (ioctl(drm_fd, 0x4600, (unsigned long)info) == 0) {
        screen_width = info[0];
        screen_height = info[1];
    } else {
        screen_width = DOOMGENERIC_RESX;
        screen_height = DOOMGENERIC_RESY;
    }

    // Allocate scaling buffer for legacy mode if needed
    if (screen_width != DOOMGENERIC_RESX || screen_height != DOOMGENERIC_RESY) {
        scaling_buffer = malloc(screen_width * screen_height * 4);
    }
}

static void fast_copy_frame(uint32_t* dest, uint32_t dest_width, uint32_t dest_height) {
    if (dest_width == DOOMGENERIC_RESX && dest_height == DOOMGENERIC_RESY) {
        // Direct memory copy for matching resolution - fastest possible
        __builtin_memcpy(dest, DG_ScreenBuffer, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);
        return;
    }

    // Fast integer-only nearest neighbor scaling for performance
    // This is much faster than bilinear interpolation
    uint32_t x_ratio = (DOOMGENERIC_RESX << 16) / dest_width;
    uint32_t y_ratio = (DOOMGENERIC_RESY << 16) / dest_height;

    for (uint32_t y = 0; y < dest_height; y++) {
        uint32_t src_y = (y * y_ratio) >> 16;
        if (src_y >= DOOMGENERIC_RESY) src_y = DOOMGENERIC_RESY - 1;

        uint32_t* src_row = &DG_ScreenBuffer[src_y * DOOMGENERIC_RESX];
        uint32_t* dest_row = &dest[y * dest_width];

        for (uint32_t x = 0; x < dest_width; x++) {
            uint32_t src_x = (x * x_ratio) >> 16;
            if (src_x >= DOOMGENERIC_RESX) src_x = DOOMGENERIC_RESX - 1;
            dest_row[x] = src_row[src_x];
        }
    }
}

// Optimized 2x scaling for common case (640x400 -> 1280x800)
static void fast_2x_scale(uint32_t* dest) {
    uint32_t* src = DG_ScreenBuffer;

    for (int y = 0; y < DOOMGENERIC_RESY; y++) {
        uint32_t* src_row = &src[y * DOOMGENERIC_RESX];
        uint32_t* dest_row1 = &dest[y * 2 * DOOMGENERIC_RESX * 2];
        uint32_t* dest_row2 = &dest[(y * 2 + 1) * DOOMGENERIC_RESX * 2];

        for (int x = 0; x < DOOMGENERIC_RESX; x++) {
            uint32_t pixel = src_row[x];

            // Write 2x2 pixel block
            dest_row1[x * 2] = pixel;
            dest_row1[x * 2 + 1] = pixel;
            dest_row2[x * 2] = pixel;
            dest_row2[x * 2 + 1] = pixel;
        }
    }
}

void DG_DrawFrame() {
    if (drm_fd < 0 || graphics_disabled) {
        // No output device available or graphics disabled - just return
        return;
    }

    if (!use_drm) {
        // Legacy framebuffer path - always write native resolution
        static int first_frame = 1;
        if (first_frame) {
            printf("DOOM: Rendering with legacy framebuffer (native %dx%d)\n", DOOMGENERIC_RESX, DOOMGENERIC_RESY);
            first_frame = 0;
        }
        // For legacy framebuffer, always write native DOOM resolution
        // Let the framebuffer console handle any scaling
        lseek(drm_fd, 0, 0);
        int legacy_result = write(drm_fd, DG_ScreenBuffer, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);

        if (legacy_result < 0) {
            // Legacy framebuffer write failed - disable graphics to prevent freeze
            printf("DOOM: Legacy framebuffer write failed - disabling graphics output\n");
            graphics_disabled = 1;
            close(drm_fd);
            drm_fd = -1;
        }
        return;
    }

    // DRM path - use hardware acceleration
    static int first_drm_frame = 1;
    if (first_drm_frame) {
        printf("DOOM: Rendering with DRM hardware acceleration (%dx%d)\n", screen_width, screen_height);
        if (double_buffered) {
            printf("DOOM: Double buffering active\n");
        }
        first_drm_frame = 0;
    }

    uint32_t* target_buffer;

    if (double_buffered) {
        target_buffer = (uint32_t*)back_fb.buffer;
    } else {
        target_buffer = (uint32_t*)primary_fb.buffer;
    }

    if (!target_buffer) {
        // No direct memory access - use hardware scaling with file operations
        static int first_fallback_msg = 1;
        if (first_fallback_msg) {
            printf("DOOM: DRM using hardware-accelerated scaling via file operations\n");
            first_fallback_msg = 0;
        }

        // Write DOOM's native resolution directly - let DRM hardware handle scaling
        // No software scaling needed - hardware will scale 320x200 to display size
        size_t bytes_to_write = DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4;
        lseek(drm_fd, 0, 0); // Seek to beginning
        int result = write(drm_fd, DG_ScreenBuffer, bytes_to_write);

        if (result >= 0) {
            // Trigger hardware scaling via page flip
            drm_flip_page();
        } else {
            // DRM write failed - fall back to /dev/fb0 with software scaling
            static int fb0_fd = -1;
            static int fallback_warned = 0;

            if (fb0_fd < 0) {
                fb0_fd = open("/dev/fb0", 1, 0); // O_WRONLY
                if (!fallback_warned) {
                    printf("DOOM: DRM write failed, falling back to /dev/fb0 with software scaling\n");
                    fallback_warned = 1;
                }
            }

            if (fb0_fd >= 0) {
                // For /dev/fb0 fallback, write native resolution and let console scale
                lseek(fb0_fd, 0, 0);
                int fb_result = write(fb0_fd, DG_ScreenBuffer, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);

                if (fb_result < 0) {
                    // All graphics output has failed - disable rendering to prevent freeze
                    static int no_graphics_warned = 0;
                    if (!no_graphics_warned) {
                        printf("DOOM: All graphics output failed - running in headless mode\n");
                        no_graphics_warned = 1;
                    }
                    // Close the failed fd and disable all further graphics attempts
                    close(fb0_fd);
                    fb0_fd = -1;
                    graphics_disabled = 1;
                }
            }
        }
        return;
    }

    // Direct memory access path with hardware scaling
    // Simply copy DOOM's native framebuffer - hardware will handle scaling
    __builtin_memcpy(target_buffer, DG_ScreenBuffer, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);

    // Flip buffers if double buffering is enabled
    if (double_buffered) {
        drm_flip_page();
        // Swap buffer references
        struct drm_framebuffer temp = primary_fb;
        primary_fb = back_fb;
        back_fb = temp;
    }
}

void DG_SleepMs(uint32_t ms) {
    // Reduce sleep time for better performance on slower systems
    // DOOM typically calls this with 14ms (for ~70 FPS)
    // We can reduce it slightly for better responsiveness
    if (ms > 10) {
        ms = ms - 2; // Slightly reduce sleep to increase framerate
    }
    usleep(ms * 1000);
}

uint32_t DG_GetTicksMs() {
    struct timespec ts;
    clock_gettime(1, &ts);
    return (uint32_t)(ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

// Enhanced input handling with DRM integration
#define KEYQUEUE_SIZE 128  // Larger queue for better responsiveness
static unsigned short s_KeyQueue[KEYQUEUE_SIZE];
static unsigned int s_KeyQueueWriteIndex = 0;
static unsigned int s_KeyQueueReadIndex = 0;

static void addKeyToQueue(int pressed, unsigned char keyCode) {
    unsigned short keyData = (pressed << 8) | keyCode;
    s_KeyQueue[s_KeyQueueWriteIndex] = keyData;
    s_KeyQueueWriteIndex++;
    s_KeyQueueWriteIndex %= KEYQUEUE_SIZE;
}

static uint32_t key_expiration[256] = {0};
static int key_is_down[256] = {0};
static int capslock_run_locked = 0;

struct leandros_input_event {
    struct {
        long long tv_sec;
        long long tv_usec;
    } time;
    unsigned short type;
    unsigned short code;
    int value;
};

void DG_SetWindowTitle(const char * title) {
    // DRM doesn't have windows, but we could potentially
    // display the title in an overlay plane
}

int DG_GetKey(int* pressed, unsigned char* key) {
    // Check queued keys first
    if (s_KeyQueueReadIndex != s_KeyQueueWriteIndex) {
        unsigned short keyData = s_KeyQueue[s_KeyQueueReadIndex];
        s_KeyQueueReadIndex++;
        s_KeyQueueReadIndex %= KEYQUEUE_SIZE;
        *pressed = keyData >> 8;
        *key = keyData & 0xFF;
        return 1;
    }

    uint32_t now = DG_GetTicksMs();

    // Handle DRM-specific input events
    if (ev_fd >= 0) {
        int bytes_avail = 0;
        if (ioctl(ev_fd, 0x541B, (unsigned long)&bytes_avail) == 0 &&
            bytes_avail >= sizeof(struct leandros_input_event)) {

            struct leandros_input_event ev;
            while (bytes_avail >= sizeof(struct leandros_input_event)) {
                if (read(ev_fd, &ev, sizeof(struct leandros_input_event)) !=
                    sizeof(struct leandros_input_event)) break;
                bytes_avail -= sizeof(struct leandros_input_event);

                if (ev.type == 1) { // EV_KEY
                    unsigned char dkey = 0;
                    if (ev.value == 2) { // Serial ASCII input
                        switch (ev.code) {
                            case '\r':
                            case '\n': dkey = KEY_ENTER; break;
                            case 0x1B: dkey = KEY_ESCAPE; break;
                            case '\t': dkey = KEY_TAB; break;
                            case 0x08:
                            case 0x7F: dkey = KEY_BACKSPACE; break;

                            // Movement keys
                            case 'w': case 'W': dkey = KEY_UPARROW; break;
                            case 's': case 'S': dkey = KEY_DOWNARROW; break;
                            case 'a': case 'A': dkey = KEY_LEFTARROW; break;
                            case 'd': case 'D': dkey = KEY_RIGHTARROW; break;

                            // Action keys
                            case ' ': dkey = ' '; break;
                            case 'e': case 'E': dkey = KEY_USE; break;
                            case 'c': case 'C': dkey = KEY_FIRE; break;
                            case ',': dkey = KEY_STRAFE_L; break;
                            case '.': dkey = KEY_STRAFE_R; break;
                            case 'r': case 'R': dkey = KEY_USE; break; // Alternative use key
                            case 'f': case 'F': dkey = KEY_FIRE; break; // Alternative fire key

                            default:
                                if (ev.code >= 'a' && ev.code <= 'z') dkey = ev.code;
                                else if (ev.code >= 'A' && ev.code <= 'Z') dkey = ev.code - 'A' + 'a';
                                else if (ev.code >= '0' && ev.code <= '9') dkey = ev.code;
                                break;
                        }
                    } else { // Keyboard scancode input (ev.value 0 or 1)
                        switch (ev.code) {
                            case 1: dkey = KEY_ESCAPE; break;
                            case 2: dkey = '1'; break;
                            case 3: dkey = '2'; break;
                            case 4: dkey = '3'; break;
                            case 5: dkey = '4'; break;
                            case 6: dkey = '5'; break;
                            case 7: dkey = '6'; break;
                            case 8: dkey = '7'; break;
                            case 9: dkey = '8'; break;
                            case 10: dkey = '9'; break;
                            case 11: dkey = '0'; break;
                            case 12: dkey = KEY_MINUS; break;
                            case 13: dkey = KEY_EQUALS; break;
                            case 14: dkey = KEY_BACKSPACE; break;
                            case 15: dkey = KEY_TAB; break;
                            case 16: dkey = 'q'; break;
                            case 17: dkey = KEY_UPARROW; break; // W
                            case 18: dkey = KEY_USE; break; // E
                            case 19: dkey = 'r'; break;
                            case 20: dkey = 't'; break;
                            case 21: dkey = 'y'; break;
                            case 22: dkey = 'u'; break;
                            case 23: dkey = 'i'; break;
                            case 24: dkey = 'o'; break;
                            case 25: dkey = 'p'; break;
                            case 28: dkey = KEY_ENTER; break;
                            case 29: dkey = KEY_FIRE; break; // LCTRL
                            case 30: dkey = KEY_LEFTARROW; break; // A
                            case 31: dkey = KEY_DOWNARROW; break; // S
                            case 32: dkey = KEY_RIGHTARROW; break; // D
                            case 33: dkey = KEY_FIRE; break; // F
                            case 42: dkey = KEY_RSHIFT; break; // LSHIFT
                            case 44: dkey = 'z'; break;
                            case 45: dkey = 'x'; break;
                            case 46: dkey = KEY_FIRE; break; // C
                            case 51: dkey = KEY_STRAFE_L; break; // ,
                            case 52: dkey = KEY_STRAFE_R; break; // .
                            case 54: dkey = KEY_RSHIFT; break; // RSHIFT
                            case 57: dkey = ' '; break; // SPACE
                            case 58: // Caps Lock
                                if (ev.value == 1) {
                                    capslock_run_locked = !capslock_run_locked;
                                    addKeyToQueue(capslock_run_locked ? 1 : 0, KEY_RSHIFT);
                                    key_is_down[KEY_RSHIFT] = capslock_run_locked;
                                }
                                dkey = 0;
                                break;
                            case 97: dkey = KEY_FIRE; break; // RCTRL
                            case 103: dkey = KEY_UPARROW; break;
                            case 108: dkey = KEY_DOWNARROW; break;
                            case 105: dkey = KEY_LEFTARROW; break;
                            case 106: dkey = KEY_RIGHTARROW; break;
                            case 182: dkey = KEY_RSHIFT; break;
                            case 186:
                                if (ev.value == 1) {
                                    capslock_run_locked = !capslock_run_locked;
                                    addKeyToQueue(capslock_run_locked ? 1 : 0, KEY_RSHIFT);
                                    key_is_down[KEY_RSHIFT] = capslock_run_locked;
                                }
                                dkey = 0;
                                break;
                            default: dkey = 0; break;
                        }
                    }

                    if (dkey != 0) {
                        if (ev.value == 1) { // Down
                            key_is_down[dkey] = 1;
                            key_expiration[dkey] = 0;
                            addKeyToQueue(1, dkey);
                        } else if (ev.value == 2) { // Repeat
                            key_expiration[dkey] = now + 150;
                            if (!key_is_down[dkey]) {
                                key_is_down[dkey] = 1;
                                addKeyToQueue(1, dkey);
                            }
                        } else if (ev.value == 0) { // Up
                            if (key_is_down[dkey]) {
                                if (dkey == KEY_RSHIFT && capslock_run_locked) {
                                    // Keep shift active if capslock is on
                                } else {
                                    key_is_down[dkey] = 0;
                                    key_expiration[dkey] = 0;
                                    addKeyToQueue(0, dkey);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Auto-release keys that have expired
    for (int i = 0; i < 256; i++) {
        if (key_is_down[i] && key_expiration[i] != 0 && now > key_expiration[i]) {
            key_is_down[i] = 0;
            key_expiration[i] = 0;
            addKeyToQueue(0, i);
        }
    }

    // Check for queued keys again
    if (s_KeyQueueReadIndex != s_KeyQueueWriteIndex) {
        unsigned short keyData = s_KeyQueue[s_KeyQueueReadIndex];
        s_KeyQueueReadIndex++;
        s_KeyQueueReadIndex %= KEYQUEUE_SIZE;
        *pressed = keyData >> 8;
        *key = keyData & 0xFF;
        return 1;
    }

    return 0;
}

int main(int argc, char **argv) {
    doomgeneric_Create(argc, argv);

    while (1) {
        doomgeneric_Tick();
    }

    // Cleanup
    if (scaling_buffer) {
        free(scaling_buffer);
    }

    if (drm_fd >= 0) {
        close(drm_fd);
    }

    if (ev_fd >= 0) {
        close(ev_fd);
    }

    return 0;
}

// Functions to expose DRM display dimensions to the video system
int DG_GetDRMDisplayWidth(void) {
    return (drm_fd >= 0) ? screen_width : 0;
}

int DG_GetDRMDisplayHeight(void) {
    return (drm_fd >= 0) ? screen_height : 0;
}

int DG_IsDRMActive(void) {
    return (drm_fd >= 0) ? 1 : 0;
}