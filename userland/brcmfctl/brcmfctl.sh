#!/bin/sh
# brcmfctl: runtime control of the Pi 5 CYW43455 (brcmf) bring-up.
# The kernel's brcmf thread polls /etc/brcmf.ctl once a second while the
# bring-up is failed and prints its answer as "[brcmf] ..." lines on the console.
#   brcmfctl retry [knob=val ...]   re-pulse WL_REG_ON, redo SDIO init, re-download
#   brcmfctl diag [lines=N]         chip/PMU/CR4 state + firmware console tail
#   brcmfctl status
# Knobs (each retry starts from the defaults): clk_ms dl_ht=0|1 post_ms ht_ms f2_ms
#   sr=auto|0|1 kso=0|1 wm=N f2wm=0|1 mesbusy=N force_ht=0|1 noclm=1
#   wloff_ms wlon_ms lines=N fw=PATH nvram=PATH clm=PATH
if [ $# -eq 0 ]; then
    echo "usage: brcmfctl retry [knob=val ...] | diag [lines=N] | status"
    exit 2
fi
# A fresh sequence number makes a repeated command count as new.
n=$(cat /etc/brcmf.ctl.n 2>/dev/null)
n=$((${n:-0} + 1))
echo "$n" > /etc/brcmf.ctl.n
echo "seq=$n.$RANDOM $*" > /etc/brcmf.ctl || exit 1
echo "brcmfctl: queued '$*' (answer within ~1 s as [brcmf] lines)"
