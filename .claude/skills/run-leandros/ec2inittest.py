#!/usr/bin/env python3
"""ec2inittest.py [aarch64|x86_64] [--keep] -- /bin/ec2init, the EC2 first-boot
service (lane ec2init), against scripts/fake-imds.py behind a slirp guestfwd
address (10.0.2.100:80; cmdline ec2=1 ec2init.endpoint=10.0.2.100:80).

Steps (`ec2init_<step>: PASS|FAIL detail`):
  fake_imdsv2     host only: the fake enforces IMDSv2 (GET without a token 401,
                  bad token 401, PUT without TTL 400, PUT + token then GET 200)
  boot_key        init ran ec2init on its own: ssh as root AND leandro with the
                  IMDS key (no password) works
  protocol        the fake saw PUT /latest/api/token first and every GET with a
                  valid token (no 401 from the guest)
  hostname        the guest hostname is the IMDS local-hostname's short label
  stamp           /var/lib/ec2-init/instance-id holds the instance id
  same_id_noop    IMDS key changed but the id did not: ec2init exits 0 "already",
                  the new key is not installed, public-keys is not fetched
  new_id          a new instance id: the new key is installed (login works),
                  the old key stays
  not_ec2_fast    no EC2 marker (EC2INIT_CMDLINE override): exits 0 in < 3 s
  unreachable     marker but no IMDS (10.0.2.99): gives up after ec2init.wait=3,
                  exit 1, nothing installed
Exit 0 iff all PASS.
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ.get("REPO", os.path.abspath(os.path.join(HERE, "../../..")))
DRV = os.path.join(HERE, "driver.py")
FAKE = os.path.join(REPO, "scripts", "fake-imds.py")
CMDLINE = "ec2=1 ec2init.endpoint=10.0.2.100:80"
sys.path.insert(0, HERE)
import imgclone  # noqa: E402


def keygen(path):
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "ec2init-test", "-f", path], check=True)
    return open(path + ".pub").read().strip()


def fake_unit(state):
    """Drive fake-imds.py on pipes; returns [(label, ok)]."""
    def req(raw):
        r = subprocess.run([sys.executable, "-I", FAKE, state], input=raw.encode(), capture_output=True, timeout=20)
        return r.stdout.decode(errors="replace")

    def code(o):
        return o.split(" ")[1] if o.startswith("HTTP/1.1 ") else "?"
    ttl = "X-aws-ec2-metadata-token-ttl-seconds: 21600\r\n"
    put = req(f"PUT /latest/api/token HTTP/1.1\r\nHost: x\r\n{ttl}Content-Length: 0\r\nConnection: close\r\n\r\n")
    tok = put.split("\r\n\r\n", 1)[-1].strip()
    get = "GET /latest/meta-data/instance-id HTTP/1.1\r\nHost: x\r\n"
    return [("no_ttl_400", code(req("PUT /latest/api/token HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")) == "400"),
            ("put_200", code(put) == "200" and len(tok) > 20),
            ("get_notoken_401", code(req(get + "Connection: close\r\n\r\n")) == "401"),
            ("get_badtoken_401", code(req(get + "X-aws-ec2-metadata-token: nope\r\nConnection: close\r\n\r\n")) == "401"),
            ("get_token_200", code(req(get + f"X-aws-ec2-metadata-token: {tok}\r\nConnection: close\r\n\r\n")) == "200")]


def main(argv):
    arch = next((a for a in argv if a in ("aarch64", "x86_64")), None)
    keep = "--keep" in argv
    arch = arch or os.environ.get("LEANDROS_TEST_ARCH") or "aarch64"
    caller = os.environ.get("LEANDROS_RUN_ID", "")
    tmp = tempfile.mkdtemp(prefix="ec2init-")
    results = {}

    def verdict(name, ok, detail=""):
        results[name] = bool(ok)
        print(f"ec2init_{name}: {'PASS' if ok else 'FAIL'} {detail}".rstrip(), flush=True)

    imds = os.path.join(tmp, "imds")
    os.makedirs(imds)
    k1, k2 = os.path.join(tmp, "k1"), os.path.join(tmp, "k2")
    pub1, pub2 = keygen(k1), keygen(k2)

    def setcfg(iid, key):
        with open(os.path.join(imds, "config.json"), "w") as f:
            json.dump({"instance_id": iid, "key": key, "hostname": "ip-10-0-7-42.ec2.internal", "user_data": ""}, f)

    def imds_log():
        try:
            return open(os.path.join(imds, "log")).read().splitlines()
        except OSError:
            return []

    setcfg("i-0aaa1111", pub1)
    unit = fake_unit(imds)
    verdict("fake_imdsv2", all(ok for _, ok in unit), " ".join(f"{n}={'ok' if ok else 'FAIL'}" for n, ok in unit))
    for f in ("log", "tokens"):
        try:
            os.unlink(os.path.join(imds, f))
        except OSError:
            pass

    img = os.path.join(tmp, f"boot-{arch}.img")
    root = os.path.join(tmp, f"root-{arch}.img")
    for src, dst in ((os.path.join(REPO, f"leandros-limine-{arch}.img"), img),
                     (os.path.join(REPO, f"f2fs-data0-{arch}.img"), root)):
        imgclone.clone_file(src, dst)
    r = subprocess.run([os.path.join(REPO, "scripts/set-image-cmdline.sh"), img, CMDLINE], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"ec2inittest: could not set the command line: {r.stdout}{r.stderr}")
        return 1
    run_id = f"{caller}-ec2init" if caller else "ec2init"
    env = dict(os.environ)
    env.update({"LEANDROS_RUN_ID": run_id, "LEANDROS_DISK_IMAGE": img, "LEANDROS_ROOT_IMAGE": root,
                "LEANDROS_NET": "user",
                "LEANDROS_GUESTFWD": f"tcp:10.0.2.100:80-cmd:{sys.executable} -I {FAKE} {imds}"})
    os.environ.update({k: v for k, v in env.items() if k.startswith("LEANDROS_")})
    sys.modules.pop("driver", None)
    import driver  # noqa: E402
    import sshsmoke  # noqa: E402
    scale = driver.wait_scale(arch, "uefi")

    def sh(c, t=60):
        res = driver.serial_run(c, timeout=t * scale, arch=arch)
        return res.get("rc"), res.get("output", "") or ""

    def keyssh(user, keyfile, cmd, tries=10):
        host, port = sshsmoke.target()
        s = sshsmoke.Ssh(host, port, tmp)
        s.key = keyfile
        rc, out, err = 255, "", ""
        for _ in range(tries):
            rc, out, err = s.run(user, cmd, key=True, timeout=60)
            if rc == 0:
                break
            time.sleep(3)
        return rc, out.strip(), err.strip()

    try:
        subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
        r = subprocess.run([sys.executable, DRV, "start", arch], env=env, capture_output=True, text=True, timeout=900)
        if r.returncode != 0:
            print(f"ec2inittest: boot failed: {r.stdout[-400:]}{r.stderr[-400:]}")
            return 1
        lg = subprocess.run([sys.executable, DRV, "login", "root", "root", str(int(90 * scale))],
                            env=env, capture_output=True, text=True, timeout=900)
        if lg.returncode != 0:
            print("ec2inittest: login failed")
            return 1
        # init starts ec2init in the background; give it time to fetch and install.
        t0 = time.time()
        while time.time() - t0 < 60 * scale and not any("public-keys/0/openssh-key" in l for l in imds_log()):
            time.sleep(2)
        rc_r, out_r, err_r = keyssh("root", k1, "echo ROOTOK; cat /var/lib/ec2-init/instance-id; hostname")
        rc_l, out_l, err_l = keyssh("leandro", k1, "echo LEANOK; id -un")
        verdict("boot_key", rc_r == 0 and "ROOTOK" in out_r and rc_l == 0 and "LEANOK" in out_l,
                f"root rc={rc_r} {err_r[-80:]!r} leandro rc={rc_l} {err_l[-80:]!r}")
        lg_ = imds_log()
        first_put = next((i for i, l in enumerate(lg_) if l.startswith("PUT /latest/api/token")), -1)
        first_get = next((i for i, l in enumerate(lg_) if l.startswith("GET ")), -1)
        verdict("protocol", first_put == 0 and first_get > first_put
                and not any("-> 401" in l or "-> 400" in l for l in lg_)
                and any("instance-id" in l and "auth=ok" in l for l in lg_),
                f"{len(lg_)} requests, first={lg_[:1]}")
        lines = out_r.splitlines()
        verdict("hostname", lines[-1:] == ["ip-10-0-7-42"], f"hostname={lines[-1:]}")
        verdict("stamp", "i-0aaa1111" in lines, f"lines={lines}")

        # same instance id, new key: a no-op
        setcfg("i-0aaa1111", pub2)
        n0 = len(imds_log())
        _, out = sh("/bin/ec2init; echo RC=$?", 90)
        new = imds_log()[n0:]
        _, auth = sh("cat /root/.ssh/authorized_keys", 30)
        verdict("same_id_noop", "RC=0" in out and "already" in out and pub1.split()[1] in auth
                and pub2.split()[1] not in auth and not any("public-keys/0" in l for l in new),
                f"out={out.strip()[-120:]!r} new={len(new)}")

        # new instance id: installed
        setcfg("i-0bbb2222", pub2)
        _, out = sh("/bin/ec2init; echo RC=$?", 90)
        rc_n, out_n, err_n = keyssh("root", k2, "echo NEWOK", 3)
        rc_o, out_o, _ = keyssh("root", k1, "echo OLDOK", 3)
        verdict("new_id", "RC=0" in out and "NEWOK" in out_n and "OLDOK" in out_o,
                f"new rc={rc_n} {err_n[-80:]!r} old rc={rc_o}")

        # no marker: fast, silent
        sh("echo quiet > /tmp/cl.empty", 30)
        t = time.time()
        _, out = sh("EC2INIT_CMDLINE=/tmp/cl.empty /bin/ec2init; echo RC=$?", 30)
        verdict("not_ec2_fast", "RC=0" in out and time.time() - t < 3 * max(scale, 1) + 5,
                f"{time.time() - t:.1f}s {out.strip()[-80:]!r}")

        # marker, endpoint dead: bounded wait, failure, no change
        sh("echo 'ec2=1 ec2init.endpoint=10.0.2.99:80 ec2init.wait=3' > /tmp/cl.dead", 30)
        setcfg("i-0ccc3333", pub1)
        t = time.time()
        _, out = sh("EC2INIT_CMDLINE=/tmp/cl.dead /bin/ec2init; echo RC=$?", 60)
        _, stamp = sh("cat /var/lib/ec2-init/instance-id", 30)
        verdict("unreachable", "RC=1" in out and "i-0bbb2222" in stamp and time.time() - t < 30 * max(scale, 1),
                f"{time.time() - t:.1f}s {out.strip()[-80:]!r}")
    finally:
        try:
            shutil.copyfile(driver.SERIAL_FULL_LOG, f"/tmp/ec2init-{arch}-serial.log")
        except Exception:  # noqa: BLE001
            pass
        if not keep:
            subprocess.run([sys.executable, DRV, "stop"], env=env, capture_output=True, timeout=120)
            shutil.rmtree(tmp, ignore_errors=True)
    ok = bool(results) and all(results.values())
    print(f"ec2inittest: {'PASS' if ok else 'FAIL'} {sum(results.values())}/{len(results)} ({arch})", flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
