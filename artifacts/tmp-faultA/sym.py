import sys
syms=[]
for line in open(sys.argv[1]):
    p=line.split()
    if len(p)<3: continue
    try:
        v=int(p[0],16); s=int(p[1],16)
    except: continue
    syms.append((v,s,p[2]))
syms.sort()
import bisect
starts=[x[0] for x in syms]
def resolve(addr):
    i=bisect.bisect_right(starts,addr)-1
    while i>=0:
        v,s,n=syms[i]
        if v<=addr and (s==0 or addr<v+s):
            return f"{n}+0x{addr-v:x}"
        # if has size and addr beyond, keep looking back only if overlapping unlikely
        if s!=0 and addr>=v+s:
            return f"{n}+0x{addr-v:x}(past end sz0x{s:x})"
        i-=1
    return "?"
for a in sys.argv[2:]:
    addr=int(a,16)
    print(f"{a}: {resolve(addr)}")
