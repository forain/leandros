#!/usr/bin/env python3
"""
Run one guest command under liveness triage, without touching the QEMU monitor.

    LEANDROS_RUN_ID=<lane> python3 liveness-run.py <label> "<guest command>" \
        [--arch aarch64|x86_64] [--mode uefi|uefi-tcg] [--timeout S] [--no-hb] \
        [--wav] [--hold]

What it does: boots, logs in as root, starts a userspace heartbeat
(`while true; do echo HB; sleep 1; done &` -- it rides on the kernel tick via
nanosleep and on fork/exec via the shell), sends the command over ONE
persistent serial connection (so nothing is dropped), and watches:

  * serial bytes, timestamped per chunk into <out>/<label>.serial-timed;
  * the heartbeat (a dead HB with audio still flowing = fork/exec or tick
    trouble; dead HB + frozen wav = the kernel tick or its hooks are gone);
  * with --wav, the QEMU wav capture size (the audio pump runs off the BSP
    tick, so a frozen file is a frozen tick or a stuck pipewire lock);
  * the guest's own [WDOG] / [TIMER] lines (sched watchdog: a CPU that took
    no timer tick for 2 s, with pid, exe, last syscall and the lock it spins
    on; arch timer self-check: a dead virtual timer, re-armed).

At a stall it does NOT run `x` or `info registers`: under HVF with the
in-kernel GIC (QEMU 11.1.1, macOS 26) an HMP command that needs run_on_cpu
can hang QEMU's main loop for good (seen 4 times on 2026-09-15, once on an
idle guest), which kills every ioeventfd device -- virtio-gpu ctrlq TIMEOUTs,
frozen audio, blocked fork/exec -- and manufactures the wedge it was meant
to diagnose. Instead it takes a host-side `sample` of the QEMU process
(which vCPU threads are spinning in hv_trap vs parked in the framework's
WFI), prints the guest's serial tail, and only then tries `info cpus` and
`cpu N; info registers` on the SPINNING vCPUs (a running vCPU is kickable)
with short timeouts, treating a timeout as "main loop dead".

Exit status 0 = command finished (prompt seen), 2 = stall.
"""
import os, sys, time, select, subprocess, re, json

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import driver  # noqa: E402

OUT = os.environ.get("LEANDROS_LIVENESS_OUT", "/tmp/leandros-liveness")
HB_CMD = "( while true; do echo HB; sleep 1; done ) &"
STALL_S = 8.0
T0 = time.time()


def log(f, msg):
    line = f"[{time.strftime('%H:%M:%S')} +{time.time()-T0:7.2f}] {msg}"
    print(line, flush=True)
    f.write(line + "\n"); f.flush()


class Monitor:
    def __init__(self, path):
        self.s = driver._connect_with_retry(path)
        self.dead = self.s is None
        if self.s is not None:
            self.s.setblocking(False)
            self._drain(0.6)

    def _drain(self, t):
        end = time.time() + t
        while time.time() < end:
            if select.select([self.s], [], [], 0.1)[0]:
                try:
                    if not self.s.recv(65536): break
                except BlockingIOError: pass

    def cmd(self, c, timeout=8):
        if self.dead: return "(monitor dead)"
        self.s.sendall((c + "\n").encode())
        buf = b""; end = time.time() + timeout
        while time.time() < end:
            if select.select([self.s], [], [], 0.1)[0]:
                try: ch = self.s.recv(65536)
                except BlockingIOError: continue
                if not ch: break
                buf += ch
                if driver._strip_ansi(buf).rstrip().endswith(b"(qemu)"): break
        else:
            self.dead = True
            return f"(TIMEOUT {timeout}s on '{c}': QEMU main loop blocked in run_on_cpu)"
        txt = driver._strip_ansi(buf).decode("utf-8", "replace")
        return "\n".join(l for l in txt.splitlines()
                         if l.strip() and l.strip() != "(qemu)" and not l.strip().startswith(c.split()[0]))


class Serial:
    def __init__(self, path, rawf, timedf):
        self.s = driver._connect_with_retry(path)
        if self.s is None: raise RuntimeError("no serial socket")
        self.s.setblocking(False)
        self.rawf, self.timedf = rawf, timedf
        self.buf = b""
        self.last_rx = time.time()
        self.last_hb = None
        self.hb_count = 0
        self.events = 0  # [WDOG]/[TIMER] lines seen

    def pump(self, t):
        end = time.time() + t
        while True:
            rem = end - time.time()
            if rem <= 0: return
            if select.select([self.s], [], [], min(0.1, rem))[0]:
                try: ch = self.s.recv(65536)
                except BlockingIOError: continue
                if not ch: return
                now = time.time()
                self.last_rx = now
                self.buf += ch
                if b"\x1b[6n" in ch:
                    self.s.sendall(b"\x1b[24;1R" * ch.count(b"\x1b[6n"))
                n = ch.count(b"HB\r\n") + ch.count(b"HB\n")
                if n:
                    self.hb_count += n; self.last_hb = now
                tail = self.buf[-len(ch) - 16:]
                self.events += tail.count(b"[WDOG]") + tail.count(b"[TIMER]")
                self.rawf.write(ch); self.rawf.flush()
                self.timedf.write(f"[{now-T0:8.2f}] {ch!r}\n"); self.timedf.flush()

    def send_paced(self, line):
        payload = line.encode()
        self.s.setblocking(True)
        for i in range(0, len(payload), 8):
            self.s.sendall(payload[i:i + 8]); time.sleep(0.02)
        self.s.setblocking(False)

    def send_cmd(self, command):
        self.send_paced("\r")
        end = time.time() + 2.0
        while time.time() < end:
            self.pump(0.1)
            if b"#" in driver._strip_ansi(self.buf)[-24:]: break
        time.sleep(0.05)
        self.send_paced("  " + command + "\n")

    def text(self):
        return driver._strip_guest_ansi(self.buf).decode("utf-8", "replace")


def host_sample(pid, secs, path):
    subprocess.run(["sample", str(pid), str(secs), "-file", path], capture_output=True)
    txt = open(path, errors="replace").read() if os.path.exists(path) else ""
    res, cur = {}, None
    for ln in txt.splitlines():
        m = re.match(r"\s*(\d+) Thread_\w+: (.*)", ln)
        if m:
            cur = m.group(2).strip(); res[cur] = {"total": int(m.group(1)), "hv_trap": 0, "cvwait": 0}; continue
        if cur and "hv_trap" in ln:
            m2 = re.match(r"[\s+!:|]*(\d+) ", ln)
            if m2: res[cur]["hv_trap"] += int(m2.group(1))
        if cur and "__psynch_cvwait" in ln:
            m2 = re.match(r"[\s+!:|]*(\d+) ", ln)
            if m2: res[cur]["cvwait"] += int(m2.group(1))
    return res


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    label, command = sys.argv[1], sys.argv[2]
    arch, mode, timeout, hb, wav, hold = "aarch64", "uefi", 120.0, True, False, False
    a = sys.argv[3:]; i = 0
    while i < len(a):
        if a[i] == "--arch": arch = a[i + 1]; i += 2
        elif a[i] == "--mode": mode = a[i + 1]; i += 2
        elif a[i] == "--timeout": timeout = float(a[i + 1]); i += 2
        elif a[i] == "--no-hb": hb = False; i += 1
        elif a[i] == "--wav": wav = True; i += 1
        elif a[i] == "--hold": hold = True; i += 1
        else: i += 1
    os.makedirs(OUT, exist_ok=True)
    f = open(f"{OUT}/{label}.log", "w")
    rawf = open(f"{OUT}/{label}.serial", "wb")
    timedf = open(f"{OUT}/{label}.serial-timed", "w")
    wav_path = f"{OUT}/{label}.wav"
    result = {"label": label, "arch": arch, "mode": mode, "command": command, "outcome": "?"}
    log(f, f"run {label} arch={arch} mode={mode} run_id={os.environ.get('LEANDROS_RUN_ID', '')!r}")

    env = dict(os.environ)
    if wav:
        env["LEANDROS_AUDIO_WAV"] = wav_path
        if os.path.exists(wav_path): os.unlink(wav_path)
    subprocess.run([sys.executable, driver.__file__, "stop"], capture_output=True)
    r = subprocess.run([sys.executable, driver.__file__, "start", arch, mode], env=env, capture_output=True, text=True)
    if r.returncode != 0:
        log(f, f"START FAILED rc={r.returncode}\n{r.stdout}\n{r.stderr}")
        result["outcome"] = "boot-failed"; return finish(f, result, 1)
    subprocess.run([sys.executable, driver.__file__, "login", "root", "root"], env=env, capture_output=True)
    qpid = driver._qemu_pid()
    ser = Serial(driver.SERIAL_SOCK, rawf, timedf)
    if hb:
        ser.send_cmd(HB_CMD); ser.pump(2.5)
        log(f, f"heartbeat started (HB so far {ser.hb_count})")
    t_cmd = time.time()
    ser.send_cmd(command)
    log(f, f"sent: {command}")

    last_wav, last_wav_t, last_report, seen_events = 0, time.time(), 0.0, 0
    stalled = done = False
    while time.time() - t_cmd < timeout:
        ser.pump(1.0)
        now = time.time()
        if wav:
            try: wsz = os.path.getsize(wav_path)
            except OSError: wsz = 0
            if wsz != last_wav: last_wav, last_wav_t = wsz, now
        if ser.events != seen_events:
            seen_events = ser.events
            for l in ser.text().split("\n")[-40:]:
                if "[WDOG]" in l or "[TIMER]" in l: log(f, f"t={now-t_cmd:6.1f} GUEST {l.strip()[:220]}")
        if now - last_report >= 5.0:
            last_report = now
            hb_age = (now - ser.last_hb) if ser.last_hb else -1
            log(f, f"t={now-t_cmd:6.1f} hb={ser.hb_count} hb_age={hb_age:4.1f} ser_age={now-ser.last_rx:4.1f}"
                   + (f" wav={last_wav} wav_age={now-last_wav_t:4.1f}" if wav else ""))
        # done: the command's echo has appeared and the prompt is back
        if command.strip().encode() in ser.buf and driver._at_prompt(ser.buf):
            done = True; break
        hb_dead = hb and ser.last_hb is not None and now - ser.last_hb > STALL_S
        wav_frozen = wav and now - last_wav_t > STALL_S
        ser_dead = now - ser.last_rx > STALL_S
        if (hb_dead and (not wav or wav_frozen)) or (not hb and ser_dead and (not wav or wav_frozen)):
            log(f, f"STALL at t={now-t_cmd:.1f}: hb_age={now-ser.last_hb if ser.last_hb else -1:.1f} ser_age={now-ser.last_rx:.1f}"
                   + (f" wav_age={now-last_wav_t:.1f}" if wav else ""))
            stalled = True; break
        if hb_dead and wav and not wav_frozen and now - ser.last_hb > 15:
            log(f, f"HEARTBEAT-ONLY STALL at t={now-t_cmd:.1f}: audio flows, no HB for {now-ser.last_hb:.1f}s")
            stalled = True; result["hb_only"] = True; break
    if not stalled and not done:
        log(f, f"TIMEOUT after {timeout}s without the prompt"); stalled = True
    if stalled:
        result["outcome"] = "STALL"
        smp = host_sample(qpid, 3, f"{OUT}/{label}.sample.txt")
        for th, v in smp.items():
            if "CPU" in th or "main" in th:
                log(f, f"  host thread {th}: total={v['total']} hv_trap={v['hv_trap']} cvwait={v['cvwait']}")
        spinning = sorted(int(re.search(r"CPU (\d+)", k).group(1)) for k, v in smp.items()
                          if "CPU" in k and v["hv_trap"] > v["total"] * 0.5)
        log(f, f"  spinning vCPUs (>50% in hv_trap): {spinning}; the rest are parked in WFI")
        lines = [l for l in ser.text().split("\n") if l.strip() and l.strip() != "HB"]
        log(f, "  guest serial tail:\n    " + "\n    ".join(lines[-25:]))
        result["guest_events"] = [l for l in lines if "[WDOG]" in l or "[TIMER]" in l][:10]
        mon = Monitor(driver.MONITOR_SOCK)
        log(f, "  info cpus: " + mon.cmd("info cpus", 5))
        result["pcs"] = {}
        for c in spinning + [c for c in range(4) if c not in spinning]:
            if mon.dead: break
            mon.cmd(f"cpu {c}", 5)
            if mon.dead: break
            r = mon.cmd("info registers", 8)
            if mon.dead:
                log(f, f"  cpu{c}: info registers HUNG (vCPU not kickable; QEMU main loop is now dead)"); break
            m = re.search(r"\b(PC|RIP)=([0-9a-f]+)", r)
            if m:
                log(f, f"  cpu{c} pc=0x{m.group(2)}  (symbolize: llvm-nm -nC target/final-{arch}/kernel)")
                result["pcs"][str(c)] = m.group(2)
        if hold:
            log(f, "HOLD: leaving QEMU up"); return finish(f, result, 2)
    else:
        result["outcome"] = "OK"
    b = ser.buf
    result.update(hb=ser.hb_count, duration=round(time.time() - t_cmd, 1),
                  wdog=b.count(b"[WDOG]"), timer=b.count(b"[TIMER]"), recoveries=b.count(b"recovering stream"))
    log(f, f"RESULT {json.dumps(result)}")
    subprocess.run([sys.executable, driver.__file__, "stop"], capture_output=True)
    time.sleep(1)
    try:
        os.kill(qpid, 0); os.kill(qpid, 9); log(f, "QEMU ignored quit/SIGTERM (main loop blocked) -> SIGKILL")
    except (ProcessLookupError, TypeError): pass
    return finish(f, result, 0 if not stalled else 2)


def finish(f, result, code):
    with open(f"{OUT}/results.jsonl", "a") as rf: rf.write(json.dumps(result) + "\n")
    f.close()
    return code


if __name__ == "__main__":
    sys.exit(main())
