import subprocess, os, time
D = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
def d(*a, t=120):
    try: r=subprocess.run(["python3",D,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
o=d("start","aarch64","uefi-hvf",t=175)
print("BOOT", "OK" if any(m in o for m in ("Login prompt","login: ","Shell ready")) else "FAIL")
print(d("login","root","root",t=45)[-60:])
for i in range(5):
    out=d("cmd","waittest",t=120)
    # extract subtest results
    for ln in out.splitlines():
        if "process_group" in ln or "SUMMARY" in ln or "WAITTEST" in ln or ("FAIL" in ln and "wait" in ln.lower()):
            print(f"run{i}: {ln.strip()[:80]}")
    print(f"run{i}: {'PASS-run' if 'WAITTEST: FAIL' not in out and 'FAIL' not in out else 'HAD-FAIL'}")
subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True)
print("WAITCHECK DONE")
