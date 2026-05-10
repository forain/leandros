#include "doomgeneric.h"
#include "leandros_libc.h"
#include "doomkeys.h"

static int fb_fd = -1;
static int ev_fd = -1;
static uint32_t screen_width, screen_height, screen_pitch;

void DG_Init() {
    fb_fd = open("/dev/fb0", 1, 0); // O_WRONLY
    ev_fd = open("/dev/input/event0", 0, 0); // O_RDONLY
    if (fb_fd < 0) {
        // Fallback to stdout if fb fails, just to see something
        return;
    }
    
    // Get screen resolution from ioctl
    uint32_t info[8];
    // 0x4600 = FBIOGET_VSCREENINFO
    if (ioctl(fb_fd, 0x4600, (unsigned long)info) == 0) {
        screen_width = info[0];
        screen_height = info[1];
        screen_pitch = info[7];
        if (screen_pitch == 0) screen_pitch = screen_width * 4;
    } else {
        // Fallback defaults
        screen_width = 640;
        screen_height = 400;
        screen_pitch = 640 * 4;
    }
}

void DG_DrawFrame() {
    if (fb_fd < 0) return;
    
    // If resolution matches and there's no padding, simple write
    if (screen_width == DOOMGENERIC_RESX && screen_height == DOOMGENERIC_RESY && screen_pitch == DOOMGENERIC_RESX * 4) {
        lseek(fb_fd, 0, 0); // SEEK_SET
        write(fb_fd, DG_ScreenBuffer, DOOMGENERIC_RESX * DOOMGENERIC_RESY * 4);
    } else {
        static uint32_t* row_buffer = NULL;
        static int* x_map = NULL;
        static uint32_t last_width = 0;
        
        if (row_buffer == NULL || last_width != screen_width) {
            if (row_buffer) free(row_buffer);
            if (x_map) free(x_map);
            
            row_buffer = malloc(screen_width * 4);
            x_map = malloc(screen_width * sizeof(int));
            
            if (x_map) {
                for (int x = 0; x < screen_width; x++) {
                    x_map[x] = x * DOOMGENERIC_RESX / screen_width;
                }
            }
            last_width = screen_width;
        }
        
        if (!row_buffer || !x_map) return;

        int last_src_y = -1;

        for (int y = 0; y < screen_height; y++) {
            int src_y = y * DOOMGENERIC_RESY / screen_height;
            
            if (src_y != last_src_y) {
                uint32_t* src_row = &DG_ScreenBuffer[src_y * DOOMGENERIC_RESX];
                for (int x = 0; x < screen_width; x++) {
                    row_buffer[x] = src_row[x_map[x]];
                }
                last_src_y = src_y;
            }
            
            lseek(fb_fd, y * screen_pitch, 0);
            write(fb_fd, row_buffer, screen_width * 4);
        }
    }
}

void DG_SleepMs(uint32_t ms) {
    // leandros-libc has usleep
    usleep(ms * 1000);
}

uint32_t DG_GetTicksMs() {
    struct timespec ts;
    clock_gettime(1, &ts);
    return (uint32_t)(ts.tv_sec * 1000 + ts.tv_nsec / 1000000);
}

#define KEYQUEUE_SIZE 64
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

struct leandros_input_event {
    struct {
        long long tv_sec;
        long long tv_usec;
    } time;
    unsigned short type;
    unsigned short code;
    int value;
};

void DG_SetWindowTitle(const char * title) {}

int DG_GetKey(int* pressed, unsigned char* key) {
    if (s_KeyQueueReadIndex != s_KeyQueueWriteIndex) {
        unsigned short keyData = s_KeyQueue[s_KeyQueueReadIndex];
        s_KeyQueueReadIndex++;
        s_KeyQueueReadIndex %= KEYQUEUE_SIZE;
        *pressed = keyData >> 8;
        *key = keyData & 0xFF;
        return 1;
    }

    uint32_t now = DG_GetTicksMs();

    if (ev_fd >= 0) {
        int bytes_avail = 0;
        // 0x541B is FIONREAD
        if (ioctl(ev_fd, 0x541B, (unsigned long)&bytes_avail) == 0 && bytes_avail >= sizeof(struct leandros_input_event)) {
            struct leandros_input_event ev;
            while (bytes_avail >= sizeof(struct leandros_input_event)) {
                if (read(ev_fd, &ev, sizeof(struct leandros_input_event)) != sizeof(struct leandros_input_event)) break;
                bytes_avail -= sizeof(struct leandros_input_event);
                
                if (ev.type == 1) { // EV_KEY
                    unsigned char dkey = 0;
                    // Simple mapping from ASCII (as pushed by kernel UART) to Doom keys
                    switch (ev.code) {
                        case '\r':
                        case '\n': dkey = KEY_ENTER; break;
                        case 0x1B: dkey = KEY_ESCAPE; break;
                        case '\t': dkey = KEY_TAB; break;
                        case 0x08:
                        case 0x7F: dkey = KEY_BACKSPACE; break;
                        case 'w':
                        case 'W': dkey = KEY_UPARROW; break;
                        case 's':
                        case 'S': dkey = KEY_DOWNARROW; break;
                        case 'a':
                        case 'A': dkey = KEY_LEFTARROW; break;
                        case 'd':
                        case 'D': dkey = KEY_RIGHTARROW; break;
                        case ' ': dkey = ' '; break;
                        case 'e':
                        case 'E': dkey = KEY_USE; break;
                        case 'f':
                        case 'F': dkey = KEY_FIRE; break;
                        default:
                            if (ev.code >= 'a' && ev.code <= 'z') dkey = ev.code;
                            else if (ev.code >= 'A' && ev.code <= 'Z') dkey = ev.code - 'A' + 'a';
                            else if (ev.code >= '0' && ev.code <= '9') dkey = ev.code;
                            break;
                    }
                    
                    if (dkey != 0) {
                        if (ev.value == 1) { // Native Down
                            key_is_down[dkey] = 1;
                            key_expiration[dkey] = 0; // Never auto-release
                            addKeyToQueue(1, dkey);
                        } else if (ev.value == 2) { // Serial Down/Repeat
                            key_expiration[dkey] = now + 150;
                            if (!key_is_down[dkey]) {
                                key_is_down[dkey] = 1;
                                addKeyToQueue(1, dkey);
                            }
                        } else if (ev.value == 0) { // Native Up
                            if (key_is_down[dkey]) {
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

    for (int i = 0; i < 256; i++) {
        if (key_is_down[i] && key_expiration[i] != 0 && now > key_expiration[i]) {
            key_is_down[i] = 0;
            key_expiration[i] = 0;
            addKeyToQueue(0, i);
        }
    }

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
    
    return 0;
}
