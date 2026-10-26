# lxmigrate (guest): point the TRIAL entry's root at partition suffix $2 (from $1). Never prints the cmdline.
from=$1; to=$2; P=root=PARTUUID=a20ccfac-
get() { t=; espctl status -v > /tmp/st.txt 2>&1
  while IFS= read -r l; do case $l in trial_cmdline=*) t=${l#trial_cmdline=};; esac; done < /tmp/st.txt
  rm -f /tmp/st.txt; }
get
case "$t" in *"$P$from"*) ;; *) echo "SETROOT FAIL: no $P$from in trial extra"; exit 1;; esac
n=${t//$P$from/$P$to}
espctl cmdline trial $n > /dev/null 2>&1 || { echo "SETROOT FAIL: espctl cmdline"; exit 1; }
get
case "$t" in *"$P$from"*) echo "SETROOT FAIL: old root still there"; exit 1;; *"$P$to"*) echo "SETROOT OK: trial root -$to";; *) echo "SETROOT FAIL: verify"; exit 1;; esac
