#ifndef _SDL_H
#define _SDL_H

#include <stdint.h>
#include <stdbool.h>

typedef struct SDL_Window SDL_Window;
typedef struct SDL_Renderer SDL_Renderer;
typedef struct SDL_Texture SDL_Texture;

typedef uint16_t Uint16;
typedef int16_t Sint16;

typedef struct SDL_version {
    uint8_t major;
    uint8_t minor;
    uint8_t patch;
} SDL_version;

typedef struct {
    int src_format;
    int dst_format;
    double rate_incr;
    uint8_t *buf;
    int len;
    int len_cvt;
    int len_mult;
    double len_ratio;
    void (*filters[10])(void);
    int filter_index;
} SDL_AudioCVT;

typedef struct SDL_Keysym {
    uint32_t sym;
} SDL_Keysym;

typedef struct SDL_KeyboardEvent {
    uint32_t type;
    SDL_Keysym keysym;
} SDL_KeyboardEvent;

typedef union SDL_Event {
    uint32_t type;
    SDL_KeyboardEvent key;
} SDL_Event;

#define SDL_QUIT 0x100
#define SDL_KEYDOWN 0x300
#define SDL_KEYUP 0x301

#define SDLK_RETURN '\r'
#define SDLK_ESCAPE 27
#define SDLK_LEFT 0x40000050
#define SDLK_RIGHT 0x4000004F
#define SDLK_UP 0x40000052
#define SDLK_DOWN 0x40000051
#define SDLK_LCTRL 0x400000E0
#define SDLK_RCTRL 0x400000E4
#define SDLK_SPACE ' '
#define SDLK_LSHIFT 0x400000E1
#define SDLK_RSHIFT 0x400000E5
#define SDLK_LALT 0x400000E2
#define SDLK_RALT 0x400000E6
#define SDLK_F2 0x4000003B
#define SDLK_F3 0x4000003C
#define SDLK_F4 0x4000003D
#define SDLK_F5 0x4000003E
#define SDLK_F6 0x4000003F
#define SDLK_F7 0x40000040
#define SDLK_F8 0x40000041
#define SDLK_F9 0x40000042
#define SDLK_F10 0x40000043
#define SDLK_F11 0x40000044
#define SDLK_EQUALS '='
#define SDLK_PLUS '+'
#define SDLK_MINUS '-'

#define SDL_WINDOW_SHOWN 0x4
#define SDL_WINDOWPOS_UNDEFINED 0
#define SDL_PIXELFORMAT_RGB888 1
#define SDL_TEXTUREACCESS_TARGET 1
#define SDL_RENDERER_ACCELERATED 2

#define SDL_INIT_AUDIO 0x00000010
#define AUDIO_S16SYS 0x8010

#define SDL_VERSIONNUM(X, Y, Z) ((X)*1000 + (Y)*100 + (Z))

int SDL_Init(uint32_t flags);
int SDL_InitSubSystem(uint32_t flags);
void SDL_QuitSubSystem(uint32_t flags);

SDL_Window* SDL_CreateWindow(const char* title, int x, int y, int w, int h, uint32_t flags);
SDL_Renderer* SDL_CreateRenderer(SDL_Window* window, int index, uint32_t flags);
SDL_Texture* SDL_CreateTexture(SDL_Renderer* renderer, uint32_t format, int access, int w, int h);
void SDL_UpdateTexture(SDL_Texture* texture, const void* rect, const void* pixels, int pitch);
void SDL_RenderClear(SDL_Renderer* renderer);
void SDL_RenderCopy(SDL_Renderer* renderer, SDL_Texture* texture, const void* srcrect, const void* dstrect);
void SDL_RenderPresent(SDL_Renderer* renderer);
int SDL_PollEvent(SDL_Event* event);
uint32_t SDL_GetTicks(void);
void SDL_Delay(uint32_t ms);
void SDL_SetWindowTitle(SDL_Window* window, const char* title);
void SDL_Quit(void);

void SDL_PauseAudio(int pause_on);
void SDL_LockAudio(void);
void SDL_UnlockAudio(void);

#endif
