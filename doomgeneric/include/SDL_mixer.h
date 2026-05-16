#ifndef _SDL_MIXER_H
#define _SDL_MIXER_H

#include <SDL.h>

typedef struct {
    int allocated;
    uint8_t *abuf;
    uint32_t alen;
    uint8_t volume;
} Mix_Chunk;

typedef struct Mix_Music Mix_Music;

#define MIX_MAX_VOLUME 128
#define MIX_CHANNEL_POST (-2)

int Mix_OpenAudio(int frequency, uint16_t format, int channels, int chunksize);
int Mix_AllocateChannels(int numchans);
int Mix_Volume(int channel, int volume);
int Mix_PlayChannelTimed(int channel, Mix_Chunk *chunk, int loops, int ticks);
void Mix_HaltChannel(int channel);
int Mix_Playing(int channel);
void Mix_CloseAudio(void);
int Mix_QuerySpec(int *frequency, uint16_t *format, int *channels);
const SDL_version *Mix_Linked_Version(void);
const char *Mix_GetError(void);
int Mix_SetPanning(int channel, uint8_t left, uint8_t right);
int Mix_UnregisterAllEffects(int channel);

int Mix_HaltMusic(void);
int Mix_VolumeMusic(int volume);
int Mix_PlayMusic(Mix_Music *music, int loops);
void Mix_SetMusicCMD(const char *command);
typedef void (*Mix_EffectFunc_t)(int chan, void *stream, int len, void *udata);
typedef void (*Mix_EffectDone_t)(int chan, void *udata);
int Mix_RegisterEffect(int chan, Mix_EffectFunc_t f, Mix_EffectDone_t d, void *arg);

void Mix_FreeMusic(Mix_Music *music);
Mix_Music *Mix_LoadMUS(const char *file);
int Mix_PlayingMusic(void);
int Mix_SetMusicPosition(double position);

#endif
