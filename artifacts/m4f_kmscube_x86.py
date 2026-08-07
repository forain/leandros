import subprocess, os, time
D = os.path.expanduser("~/code/leandros/.claude/skills/run-leandros/driver.py")
OUT = os.path.expanduser("~/code/leandros-artifacts/notes/m4-screenshots")
def d(*a, t=120):
    try: r=subprocess.run(["python3",D,*a],capture_output=True,text=True,timeout=t); return (r.stdout or "")+(r.stderr or "")
    except subprocess.TimeoutExpired: return f"(TIMEOUT {' '.join(a)})"
subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True); time.sleep(2)
o=d("start","x86_64","uefi",t=175)
print("BOOT","OK" if any(m in o for m in ("Login prompt","login: ","Shell ready")) else "FAIL")
print(d("login","root","root",t=45)[-60:])
# no setsid: brush job-control background
d("cmd","kmscube -D /dev/dri/card0 >/tmp/kc.log 2>&1 &", t=8)
print(d("cmd","sleep 6; echo ==KC==; tail -n 4 /tmp/kc.log", t=20)[-400:])
d("screenshot", f"{OUT}/m4f-kmscube-x86_64-A.ppm", t=30)
d("cmd","sleep 1.5", t=6)
d("screenshot", f"{OUT}/m4f-kmscube-x86_64-B.ppm", t=30)
try:
    a=open(f"{OUT}/m4f-kmscube-x86_64-A.ppm",'rb').read(); b=open(f"{OUT}/m4f-kmscube-x86_64-B.ppm",'rb').read()
    diff=sum(1 for x,y in zip(a,b) if x!=y)
    print(f"KMSCUBE_X86 FRAME_DIFF_BYTES={diff} (of {min(len(a),len(b))}) -> {'ANIMATING' if diff>10000 else 'STATIC'}")
except Exception as e: print("diff err",e)
subprocess.run(["pkill","-9","-f","qemu-system"],capture_output=True)
print("KMSCUBE_X86 DONE")
