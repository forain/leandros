# hv.sh <dev> <skip 4M blocks> <count> : hash with full blocks
echo "HV start $(date)"
dd if=$1 bs=4M skip=$2 count=$3 iflag=fullblock 2>/tmp/hv.err | sha256sum
tail -n 1 /tmp/hv.err
echo "HV done $(date)"
