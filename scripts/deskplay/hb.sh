#!/bin/bash
# Mac-side 1 Hz SSH heartbeat for $1 seconds: persistent session + ping; output in $2.*
D=$2; dur=$1; K=~/.ssh/leandros_remote
ping -i 1 -t $dur 172.16.149.179 > $D.ping 2>&1 &
timeout $((dur+5)) ssh -i $K -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=10 root@172.16.149.179 "i=0; while [ \$i -lt $dur ]; do cut -d' ' -f1 /proc/uptime; sleep 1; i=\$((i+1)); done" 2>&1 \
 | perl -MTime::HiRes=time -ne 'BEGIN{$|=1} printf "%.3f %s", time, $_' > $D.ssh
wait
