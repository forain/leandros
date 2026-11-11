/* Host test: cc -o /tmp/t snd-name-test.c && /tmp/t */
#include <assert.h>
#include "snd-name.h"
static int fails;
static void chk(const char *info, const char *d, const char *n)
{
	char dd[128], nn[64];
	snd_describe(info, dd, sizeof dd, nn, sizeof nn);
	if (strcmp(dd, d) || strcmp(nn, n)) {
		printf("FAIL: got '%s'/'%s' want '%s'/'%s'\n", dd, nn, d, n);
		fails++;
	}
}
int main(void)
{
	chk("backend: virtio-sound\n", "Virtio Sound Output", "Virtio Sound");
	chk("backend: hda\n[HDA] controller 8086:2668 at 00:03.0\n[HDA] codec 0: 1af4:0022 rev 0x0 subsystem 00000000\n",
	    "Intel HD Audio Output", "Intel HD Audio");
	chk("backend: hda\n[HDA] codec 0: 10ec:0887 rev 0x1 subsystem 1\x20(Realtek ALC887)\n[HDA] DACs\n",
	    "Realtek ALC887 Analog Output (Intel HD Audio)", "Realtek ALC887");
	chk("backend: hdmi-pi5\n", "hdmi-pi5 Output", "hdmi-pi5");
	chk("backend: none\n", "Built-in Audio Output", "Built-in Audio");
	printf(fails ? "RED %d\n" : "GREEN\n", fails);
	return fails != 0;
}
