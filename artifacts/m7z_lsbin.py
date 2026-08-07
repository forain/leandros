import subprocess, os, time
DRIVER = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
def d(*a, t=120):
    r = subprocess.run(["python3", DRIVER, *a], capture_output=True, text=True, timeout=t)
    return (r.stdout or "")+(r.stderr or "")
subprocess.run(["python3", DRIVER, "stop"], capture_output=True)
subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True); time.sleep(2)
print("BOOT:", d("start","aarch64","uefi", t=200)[-200:])
d("login","root","root", t=45)
print(d("session","6",
    "ls -la /bin/cosmic-idle /bin/cosmic-greeter /bin/cosmic-panel /bin/cosmic-bg",
    "ls -la /usr/lib/libpam.so.0",
    "echo LSDONE", t=60))
subprocess.run(["python3", DRIVER, "stop"], capture_output=True)
subprocess.run(["pkill","-9","-f","qemu-system"], capture_output=True)
