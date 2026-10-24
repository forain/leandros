cat /proc/epollstat > /tmp/e0; cat /proc/wakestat > /tmp/w0
sleep 10
cat /proc/epollstat > /tmp/e1; cat /proc/wakestat > /tmp/w1
echo ==E0; cat /tmp/e0; echo ==E1; cat /tmp/e1; echo ==W0; cat /tmp/w0; echo ==W1; cat /tmp/w1
echo ==PS; for p in /proc/[0-9]*; do echo "${p#/proc/} $(cat $p/comm 2>/dev/null) $(cat $p/cmdline 2>/dev/null | tr '\0' ' ' | cut -c1-90)"; done
echo ==END
