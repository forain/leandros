#!/bin/sh
# Fake leandros-install for GUI tests. Honors the same CLI as the real backend.
case "$1" in
list)
  echo '{"ev":"disk","dev":"/dev/nvme0n1","model":"Fake NVMe 512G","size":549755813888,"eligible":true,"reason":"","extra":1}'
  echo '{"ev":"disk","dev":"/dev/sda","model":"Boot USB stick","size":34359738368,"eligible":false,"reason":"boot disk"}'
  echo '{"ev":"disk","dev":"/dev/vda","model":"virtio","size":21474836480,"eligible":true}'
  ;;
install)
  echo '{"ev":"note","msg":"unknown events are ignored"}'
  for s in gpt esp root home log verify; do
    for p in 0 50 100; do
      echo "{\"ev\":\"step\",\"name\":\"$s\",\"pct\":$p}"
      sleep "${FAKE_DELAY:-0.2}"
    done
  done
  echo '{"ev":"done","root_partuuid":"00000000-1111-2222-3333-444444444444"}'
  ;;
*) echo '{"ev":"error","msg":"bad command"}'; exit 2 ;;
esac
