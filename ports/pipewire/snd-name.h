/* Derive the PipeWire sink's node.description / node.nick from the text the
 * audio device returns for ioctl 0x103 (SND_GET_INFO): its first line is
 * "backend: virtio-sound" | "backend: hda" | module text; the HDA summary has
 * "[HDA] codec N: vvvv:dddd ... (Realtek ALC887)". Header-only so a host test
 * can compile it. */
#ifndef LEANDROS_SND_NAME_H
#define LEANDROS_SND_NAME_H
#include <stdio.h>
#include <string.h>

static inline void snd_describe(const char *info, char *desc, size_t dlen,
				char *nick, size_t nlen)
{
	const char *b = info ? strstr(info, "backend:") : NULL;
	if (b != NULL) {
		b += 8;
		while (*b == ' ')
			b++;
	}
	if (b != NULL && strncmp(b, "hda", 3) == 0) {
		const char *c = strstr(info, "[HDA] codec");
		const char *open = c ? strchr(c, '(') : NULL;
		const char *eol = c ? strchr(c, '\n') : NULL;
		const char *close = open ? strchr(open, ')') : NULL;
		if (open && close && (!eol || close < eol)) {
			snprintf(desc, dlen, "%.*s Analog Output (Intel HD Audio)",
				 (int)(close - open - 1), open + 1);
			snprintf(nick, nlen, "%.*s", (int)(close - open - 1), open + 1);
		} else {
			snprintf(desc, dlen, "Intel HD Audio Output");
			snprintf(nick, nlen, "Intel HD Audio");
		}
	} else if (b == NULL || strncmp(b, "virtio", 6) == 0) {
		snprintf(desc, dlen, "Virtio Sound Output");
		snprintf(nick, nlen, "Virtio Sound");
	} else {
		size_t n = strcspn(b, "\n");
		if (n == 0 || strncmp(b, "none", 4) == 0) {
			snprintf(desc, dlen, "Built-in Audio Output");
			snprintf(nick, nlen, "Built-in Audio");
		} else {
			snprintf(desc, dlen, "%.*s Output", (int)n, b);
			snprintf(nick, nlen, "%.*s", (int)n, b);
		}
	}
}
#endif
