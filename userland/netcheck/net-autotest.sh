#!/bin/sh
# /bin/net-autotest — the boot-time network self-test. init starts it in the
# background when the kernel command line has net.test=1 (the NVIDIA test
# stick sets it by default); everything goes to the console, which the
# real-PC boot persists to the stick's log partition.
# POSIX sh (/bin/sh is brush). Bounded: netcheck waits at most
# NET_AUTOTEST_LINK_WAIT + NET_AUTOTEST_DHCP_WAIT seconds for the network,
# then every check has its own timeout; nothing here loops forever.
#
#   NET_AUTOTEST_DELAY=N      seconds to wait before starting (default 2)
#   NET_AUTOTEST_LINK_WAIT=N  seconds netcheck waits for link (default 20)
#   NET_AUTOTEST_DHCP_WAIT=N  seconds netcheck waits for a DHCP lease (default 40)
#   NET_AUTOTEST_ARGS=...     extra netcheck arguments (e.g. --no-download)
#
# Read the result with:  grep -a 'NETCHECK\|net-autotest\|r8169\|\[NET\]' <log>

echo "==== net-autotest: start ===="
sleep "${NET_AUTOTEST_DELAY:-2}"

if [ ! -x /bin/netcheck ]; then
    echo "net-autotest: /bin/netcheck is not installed"
    echo "==== net-autotest: done (no netcheck) ===="
    exit 127
fi

# shellcheck disable=SC2086
/bin/netcheck --link-wait "${NET_AUTOTEST_LINK_WAIT:-20}" --dhcp-wait "${NET_AUTOTEST_DHCP_WAIT:-40}" \
    ${NET_AUTOTEST_ARGS:-} 2>&1
rc=$?
echo "==== net-autotest: netcheck exit code $rc ===="

# A second look a little later: counters still moving, link still up.
sleep 5
echo "net-autotest: /proc/net/nic 5 s later:"
/bin/netcheck --nic 2>&1
echo "==== net-autotest: done ===="
exit $rc
