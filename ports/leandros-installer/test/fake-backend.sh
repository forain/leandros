#!/bin/sh
# Fake leandros-install for GUI tests. Emits the same JSON events as the real backend.
# FAKE_GATE_FAIL=1 makes the dual-boot gates fail (G8 AC power); FAKE_DELAY sets step pacing.
D="${FAKE_DELAY:-0.2}"
OLD='[{"n":1,"first":2048,"last":1050623,"type":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B","guid":"1","attrs":"0000000000000000","name":"EFI"},{"n":2,"first":1050624,"last":41943039,"type":"0FC63DAF-8483-4772-8E79-3D69D8477DE4","guid":"2","attrs":"0000000000000000","name":"root"}]'
NEW='[{"n":1,"first":2048,"last":1050623,"type":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B","guid":"1","attrs":"0000000000000000","name":"EFI"},{"n":2,"first":1050624,"last":41943039,"type":"0FC63DAF-8483-4772-8E79-3D69D8477DE4","guid":"2","attrs":"0000000000000000","name":"root"},{"n":3,"first":41943040,"last":42991615,"type":"0E02ED9A","guid":"3","attrs":"8000000000000000","name":"LEANDROS boot"},{"n":4,"first":42991616,"last":76546047,"type":"3F2D991B","guid":"4","attrs":"8000000000000000","name":"LEANDROS root"},{"n":5,"first":76546048,"last":80740351,"type":"F936DEE1","guid":"5","attrs":"8000000000000000","name":"LEANDROS home"},{"n":6,"first":80740352,"last":80871423,"type":"3EC8FA02","guid":"6","attrs":"8000000000000000","name":"LEANDROS-LOG"}]'
gate() { echo "{\"ev\":\"gate\",\"id\":\"$1\",\"name\":\"$2\",\"status\":\"$3\",\"msg\":\"$4\"}"; }
gates() {
  gate G1 stick pass "target is not the boot stick"
  gate G2 disk pass "valid GPT, one ESP"
  gate G5 size pass "gap 38 GiB"
  gate G6 esp pass "systemd-boot layout"
  gate G7 backup pass "space on the stick"
  if [ -n "$FAKE_GATE_FAIL" ]; then
    gate G8 power fail "on battery, connect AC power"
    echo '{"ev":"error","msg":"preflight failed: G8"}'; exit 1
  fi
  gate G8 power pass "AC power"
  gate G3 ext4 na "not checked in free-space mode"
}
steps() { for s in "$@"; do echo "{\"ev\":\"info\",\"msg\":\"$s\"}"; for p in 0 50 100; do echo "{\"ev\":\"step\",\"name\":\"$s\",\"pct\":$p}"; sleep "$D"; done; done; }
case "$1" in
list)
  echo '{"ev":"disk","name":"nvme0n1","dev":"/dev/nvme0n1","size":549755813888,"model":"Fake NVMe 512G","bus":"nvme","removable":false,"ro":false,"boot":false,"eligible":true,"reason":"","parts":[],"gaps":[{"start":41943040,"sectors":80000000}],"dualboot":true,"dualboot_reason":""}'
  echo '{"ev":"disk","name":"nvme1n1","dev":"/dev/nvme1n1","size":274877906944,"model":"Fake NVMe full","bus":"nvme","removable":false,"ro":false,"boot":false,"eligible":true,"reason":"","parts":[],"gaps":[],"dualboot":false,"dualboot_reason":"no unallocated gap of 19 GiB","shrink":{"part":"/dev/nvme1n1p2","cur_bytes":214748364800,"min_bytes":53687091200}}'
  echo '{"ev":"disk","name":"sda","dev":"/dev/sda","size":34359738368,"model":"Boot USB stick","bus":"sd","removable":true,"ro":false,"boot":true,"eligible":false,"reason":"boot disk","parts":[{"n":1,"kind":"esp","size":536870912}]}'
  echo '{"ev":"disk","name":"vda","dev":"/dev/vda","size":21474836480,"model":"","bus":"virtio","removable":false,"ro":false,"boot":false,"eligible":true,"reason":"","parts":[],"gaps":[],"dualboot":false,"dualboot_reason":"no ESP"}'
  ;;
install)
  case " $* " in
  *" --mode dualboot "*)
    case " $* " in *" --reinstall "*)
      gate R3 set pass "one boot, root, home and log partition with bit 63"
      gate R4 markers pass "markers match"
      [ -n "$FAKE_GATE_FAIL" ] && { gate R2 mounts fail "a partition of the target is mounted"; echo '{"ev":"error","msg":"preflight failed: R2"}'; exit 1; }
      echo '{"ev":"plan","mode":"reinstall","target":"/dev/nvme0n1","keep_home":true,"root_partuuid":"00000000-1111-2222-3333-444444444444","partitions":[{"role":"boot","n":3,"action":"reformat and rewrite"},{"role":"root","n":4,"action":"block copy of the running root"},{"role":"home","n":5,"action":"keep"},{"role":"log","n":6,"action":"reformat"}],"esp_files":[{"path":"/EFI/leandros/BOOTAA64.EFI","bytes":1500000}]}'
      case " $* " in *" --dry-run "*) echo '{"ev":"done","dry_run":true}'; exit 0;; esac
      steps backup boot log root verify esp
      echo '{"ev":"done","target":"/dev/nvme0n1","mode":"reinstall","root_partuuid":"00000000-1111-2222-3333-444444444444","boot_partuuid":"cc","backup_dir":"/home/dualboot-backup/bk-1"}'
      exit 0;;
    esac
    gates
    echo "{\"ev\":\"plan\",\"mode\":\"dualboot\",\"target\":\"/dev/nvme0n1\",\"root_partuuid\":\"00000000-1111-2222-3333-444444444444\",\"old_gpt\":$OLD,\"new_gpt\":$NEW,\"esp_files\":[{\"path\":\"/EFI/leandros/BOOTAA64.EFI\",\"bytes\":1500000},{\"path\":\"/EFI/leandros/limine.conf\",\"bytes\":120},{\"path\":\"/loader/entries/leandros.conf\",\"bytes\":90},{\"path\":\"/EFI/leandros/.leandros-owned\",\"bytes\":1}]}"
    case " $* " in *" --dry-run "*) echo '{"ev":"done","dry_run":true}'; exit 0;; esac
    steps backup gpt boot log home root verify esp
    echo '{"ev":"done","target":"/dev/nvme0n1","root_partuuid":"00000000-1111-2222-3333-444444444444","disk_guid":"aaaa","root_sha256":"bbbb","boot_partuuid":"cc","backup_dir":"/home/dualboot-backup/bk-1"}'
    ;;
  *)
    echo '{"ev":"plan","target":"/dev/vda","bus":"virtio","size":21474836480,"root_partuuid":"00000000-1111-2222-3333-444444444444","nvme_rw":false}'
    steps gpt esp log home root verify
    echo '{"ev":"done","target":"/dev/vda","root_partuuid":"00000000-1111-2222-3333-444444444444","disk_guid":"aaaa","root_sha256":"bbbb"}'
    ;;
  esac
  ;;
shrink)
  case " $* " in *" --recover "*) steps recover; touch /tmp/fake-recovered; echo '{"ev":"done","recovered":true}'; exit 0;; esac
  gate G1 stick pass "ok"
  gate G3 ext4 pass "fsck clean"
  gate G4 backup pass "undo file written"
  [ -n "$FAKE_GATE_FAIL" ] && { gate G9 mounted fail "partition is mounted"; echo '{"ev":"error","msg":"preflight failed: G9"}'; exit 1; }
  steps backup check resize verify
  echo '{"ev":"done","target":"/dev/nvme1n1p2"}'
  ;;
shrink-status)
  if [ -n "$FAKE_INTERRUPTED" ] && [ ! -e /tmp/fake-recovered ]; then echo '{"ev":"shrink_status","state":"interrupted","phase":"fscking"}'; exit 0; fi
  echo '{"ev":"shrink_status","state":"shrunk-unverified"}' ;;
*) echo '{"ev":"error","msg":"bad command"}'; exit 1 ;;
esac
