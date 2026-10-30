#!/bin/sh
# Fake leandros-install for GUI tests. Emits the same JSON events as the real backend.
case "$1" in
list)
  echo '{"ev":"disk","name":"nvme0n1","dev":"/dev/nvme0n1","size":549755813888,"model":"Fake NVMe 512G","bus":"nvme","removable":false,"ro":false,"boot":false,"eligible":true,"reason":"","parts":[]}'
  echo '{"ev":"disk","name":"sda","dev":"/dev/sda","size":34359738368,"model":"Boot USB stick","bus":"sd","removable":true,"ro":false,"boot":true,"eligible":false,"reason":"boot disk","parts":[{"n":1,"kind":"esp","size":536870912}]}'
  echo '{"ev":"disk","name":"vda","dev":"/dev/vda","size":21474836480,"model":"","bus":"virtio","removable":false,"ro":false,"boot":false,"eligible":true,"reason":"","parts":[]}'
  ;;
install)
  echo '{"ev":"plan","target":"/dev/vda","bus":"virtio","size":21474836480,"root_partuuid":"00000000-1111-2222-3333-444444444444","nvme_rw":false}'
  for s in gpt esp log home root verify; do
    echo "{\"ev\":\"info\",\"msg\":\"$s\"}"
    for p in 0 50 100; do
      echo "{\"ev\":\"step\",\"name\":\"$s\",\"pct\":$p}"
      sleep "${FAKE_DELAY:-0.2}"
    done
  done
  echo '{"ev":"done","target":"/dev/vda","root_partuuid":"00000000-1111-2222-3333-444444444444","disk_guid":"aaaa","root_sha256":"bbbb"}'
  ;;
*) echo '{"ev":"error","msg":"bad command"}'; exit 1 ;;
esac
