#!/bin/bash
# ours.sh <tag>: LeandrOS side of the comparison, same sizes as the Linux init:
# AP2 6 MiB x3, control 24 MiB x2, pushed over ssh (laptop RX), with /proc/net/wifi + nic snapshots.
D=$(cd $(dirname $0)/.. && pwd); T=$1; O=$D/results-ours/$T; mkdir -p $O
H=172.16.149.179; S="ssh -i $HOME/.ssh/leandros_remote -o UserKnownHostsFile=$HOME/.ssh/leandros_known_hosts -o ConnectTimeout=8 -o BatchMode=yes root@$H"
filt() { sed -E 's/(psk|pass[a-z]*|secret|key)[=: ][^ ]*/\1=<r>/Ig'; }
U=9cbc2450-5216-453d-8c7e-84a3c682540c
echo "$T $(date +%T) $(timeout 15 $S 'uname -v' 2>&1 | tail -1)" >> $O/sum.txt
[ -f $D/blob6 ] || head -c 6291456 /dev/urandom > $D/blob6; [ -f $D/blob24 ] || head -c 25165824 /dev/urandom > $D/blob24
one() { B=$1; L=$2; N=$3; F=$4; SZ=$5
  ok=0; for try in 1 2 3; do timeout 30 $S "netctl connect --uuid $U --bssid $B" > /dev/null 2>&1
    for i in $(seq 1 15); do sleep 4; timeout 10 $S 'cat /proc/net/nic' 2>/dev/null | grep -a '^bssid' | grep -qi "$B" && { ok=1; break 2; }; done; done
  [ $ok = 1 ] || { echo "$L NOT ON $B" >> $O/sum.txt; return; }
  sleep 5; timeout 20 $S 'cat /proc/net/wifi; echo ==nic; cat /proc/net/nic' 2>/dev/null | filt > $O/$L.before.txt
  for r in $(seq 1 $N); do
    ( ping -c 12 -i 1 $H 2>/dev/null | tail -1 > $O/ping.$L.$r ) & PP=$!
    t1=$(python3 -c 'import time;print(time.time())'); timeout 120 $S 'cat > /dev/null' < $F; rc=$?; t2=$(python3 -c 'import time;print(time.time())')
    wait $PP; python3 -c "print('$L run$r %.2f Mbit/s rc=$rc ping %s'%($SZ*8/($t2-$t1)/1e6, open('$O/ping.$L.$r').read().strip()))" >> $O/sum.txt
  done
  timeout 20 $S 'cat /proc/net/wifi; echo ==nic; cat /proc/net/nic' 2>/dev/null | filt > $O/$L.after.txt
}
one 74:83:c2:ba:21:df ap2 3 $D/blob6 6291456
one 84:78:48:d8:7e:79 ctl 2 $D/blob24 25165824
