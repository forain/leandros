#!/bin/sh
# /bin/snd-autotest — boot-time sound self-test, started by init when the
# kernel command line has snd.test=1 (scripts/nvidia-test-stick.sh
# --cmdline snd.test=1). Everything goes to the console, which a real-PC boot
# persists to the USB stick's log partition. POSIX sh (/bin/sh is brush).
#
# What it does: prints the driver state (backend, HDA controller + codec ids,
# chosen output pins and their jack sense), then plays through /dev/pipewire —
# the device PipeWire's sink writes — a 2 s 440 Hz tone on both channels,
# 1 s left only (440 Hz), 1 s right only (660 Hz) and 1 s at 44.1 kHz, and
# reports DMA progress and writer starvation. Then the summary line
# 'sndtest: SUMMARY PASS|FAIL'. Bounded: nothing here waits forever.
#
#   SND_AUTOTEST_DELAY=N   seconds to wait before starting (default 5)
#   SND_AUTOTEST_REPEAT=N  play the tone set N times (default 2), so there is
#                          time to walk to the speakers / swap jacks

echo "==== snd-autotest: start ===="
sleep "${SND_AUTOTEST_DELAY:-5}"

if [ ! -e /dev/pipewire ]; then
    echo "snd-autotest: /dev/pipewire is missing: no sound driver bound."
    echo "snd-autotest: look for '[HDA]' and '[PW]' lines in the kernel log."
    echo "==== snd-autotest: FAIL (no sound device) ===="
    exit 1
fi
if [ ! -x /bin/sndtest ]; then
    echo "==== snd-autotest: FAIL (/bin/sndtest is not installed) ===="
    exit 1
fi

n=${SND_AUTOTEST_REPEAT:-2}
i=1
rc=1
while [ "$i" -le "$n" ]; do
    echo "---- snd-autotest: round $i of $n (listen: tone, left, right, tone) ----"
    /bin/sndtest all 2>&1
    rc=$?
    i=$((i + 1))
done

echo "==== snd-autotest: done (sndtest exit code $rc) ===="
exit "$rc"
