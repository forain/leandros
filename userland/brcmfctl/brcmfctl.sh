#!/bin/sh
# brcmfctl: runtime control of the Pi 5 CYW43455 (brcmf) bring-up.
# The kernel's brcmf thread polls /etc/brcmf.ctl once a second while the
# bring-up is failed and prints its answer as "[brcmf] ..." lines on the console.
#   brcmfctl retry [knob=val ...]   re-pulse WL_REG_ON, redo SDIO init, re-download
#   brcmfctl diag [lines=N]         chip/PMU/CR4 state + firmware console tail
#   brcmfctl status
#   brcmfctl dp off|rx|on [yield]  data-path bisection: off = associate but hold wifi0's carrier down,
#                                   rx = carrier up, drop every transmit; yield = sleep 1 ms per pass
#   brcmfctl peek addr=0x...        16 bytes at a backplane address (word reads + one bulk read)
#   brcmfctl poke addr=0x... val=0x...   one 32-bit backplane write
# Knobs (each retry starts from the defaults): clk_ms dl_ht=0|1 post_ms ht_ms f2_ms
#   sr=auto|0|1 kso=0|1 wm=N f2wm=0|1 mesbusy=N force_ht=0|1 noclm=1
#   wloff_ms wlon_ms lines=N fw=PATH nvram=PATH clm=PATH
#   verify=0|1|2 (off, head/tail, whole image) win4b=0|1
#   SDIO bus: sdclk=kHz hs=0|1 blk=0|1 blksz=N (F1) chunk=N (max bytes per CMD53)
if [ $# -eq 0 ]; then
    echo "usage: brcmfctl status | scan | join [ssid=NAME] | diag [lines=N] | retry [knob=val ...] | peek addr=0x.. | poke addr=0x.. val=0x.. | dp on|off|rx [yield|noyield]"
    exit 2
fi
# A fresh sequence number makes a repeated command count as new.
n=$(cat /etc/brcmf.ctl.n 2>/dev/null)
n=$((${n:-0} + 1))
echo "$n" > /etc/brcmf.ctl.n
echo "seq=$n.$RANDOM $*" > /etc/brcmf.ctl || exit 1
echo "brcmfctl: queued '$*' (answer within ~1 s as [brcmf] lines: see the log section of /proc/net/nic)"
