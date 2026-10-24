import sys,re,collections
# names of syscalls (aarch64 generic)
SC={7:'poll?',22:'epoll_pwait',73:'ppoll',98:'futex',63:'read',64:'write',222:'mmap',215:'munmap',226:'mprotect',101:'nanosleep',115:'clock_nanosleep',113:'clock_gettime',72:'pselect6',29:'ioctl',212:'recvmsg',211:'sendmsg',441:'epoll_pwait2',124:'sched_yield',233:'madvise',216:'mremap',57:'close',85:'timerfd_create',86:'timerfd_settime'}
snaps=[];cur=None;names={}
for l in open(sys.argv[1],errors='replace'):
    w=l.split()
    if not w: continue
    if w[0]=='===': cur={'name':w[2],'up':float(w[3][3:]),'P':{},'s':{},'t':{}};snaps.append(cur);continue
    if w[0]=='N' and len(w)>=4: names[int(w[1])]=' '.join(w[3:]);continue
    if cur is None: continue
    try:
        if w[0]=='P': cur['P'][int(w[1])]=(w[2],int(w[3]),int(w[4]))
        elif w[0]=='s': cur['s'][(int(w[1]),int(w[2]))]=list(map(int,w[3:7]))
        elif w[0]=='t': cur['t'][int(w[1])]=(int(w[2]),int(w[3]),int(w[4]))
    except: pass
HZ=100
pairs=[(0,1),(2,3),(4,5)]
for a,b in pairs:
    if b>=len(snaps): break
    A,B=snaps[a],snaps[b];dt=B['up']-A['up']
    print(f"--- {A['name']}->{B['name']} dt={dt:.0f}s")
    for p,(k,u,s) in sorted(B['P'].items()):
        if p not in A['P']: continue
        du=sum((c-A['t'][t][1])/1e9 for t,(tg,c,y) in B['t'].items() if tg==p and t in A['t'])
        top=[]
        for (tg,nr),v in B['s'].items():
            if tg!=p: continue
            o=A['s'].get((tg,nr),[0,0,0,0]);dc=v[0]-o[0]
            if dc>0: top.append((dc,nr,(v[1]-o[1])/1e9,v[2]-o[2]))
        top.sort(reverse=True)
        ts=' '.join(f"{SC.get(nr,nr)}:{dc/dt:.0f}/s(z{dz/dt:.0f},{ns/dt*100:.1f}%)" for dc,nr,ns,dz in top[:4])
        thr=[]
        for tid,(tg,c,y) in B['t'].items():
            if tg==p and tid in A['t']: thr.append(((c-A['t'][tid][1])/1e9/dt*100,tid))
        thr.sort(reverse=True)
        th=' '.join(f"{names.get(t,t)}={c:.0f}%" for c,t in thr[:3] if c>=2)
        print(f"{p:5d} {k:8s} cpu={du/dt*100:5.1f}% | {ts} | {th}")
