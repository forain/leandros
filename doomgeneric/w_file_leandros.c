#include "m_misc.h"
#include "w_file.h"
#include "z_zone.h"
#include "leandros_libc.h"

typedef struct
{
    wad_file_t wad;
    int fd;
} leandros_wad_file_t;

extern wad_file_class_t leandros_wad_file;

static wad_file_t *W_Leandros_OpenFile(char *path)
{
    leandros_wad_file_t *result;
    int fd;

    fd = open(path, 0, 0); // O_RDONLY

    if (fd < 0)
    {
        return NULL;
    }

    result = Z_Malloc(sizeof(leandros_wad_file_t), PU_STATIC, 0);
    result->wad.file_class = &leandros_wad_file;
    result->wad.mapped = NULL;
    
    // Get file length
    long current = lseek(fd, 0, 1); // SEEK_CUR
    long length = lseek(fd, 0, 2); // SEEK_END
    lseek(fd, current, 0); // SEEK_SET
    
    result->wad.length = (unsigned int)length;
    result->fd = fd;

    return &result->wad;
}

static void W_Leandros_CloseFile(wad_file_t *wad)
{
    leandros_wad_file_t *leandros_wad = (leandros_wad_file_t *) wad;
    close(leandros_wad->fd);
    Z_Free(leandros_wad);
}

size_t W_Leandros_Read(wad_file_t *wad, unsigned int offset,
                   void *buffer, size_t buffer_len)
{
    leandros_wad_file_t *leandros_wad = (leandros_wad_file_t *) wad;
    lseek(leandros_wad->fd, offset, 0); // SEEK_SET
    return read(leandros_wad->fd, buffer, buffer_len);
}

wad_file_class_t leandros_wad_file = 
{
    W_Leandros_OpenFile,
    W_Leandros_CloseFile,
    W_Leandros_Read,
};

// Also need to provide W_OpenFile, W_CloseFile, W_Read
wad_file_t *W_OpenFile(char *path)
{
    return W_Leandros_OpenFile(path);
}

void W_CloseFile(wad_file_t *wad)
{
    wad->file_class->CloseFile(wad);
}

size_t W_Read(wad_file_t *wad, unsigned int offset,
              void *buffer, size_t buffer_len)
{
    return wad->file_class->Read(wad, offset, buffer, buffer_len);
}
