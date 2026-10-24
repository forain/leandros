import sys,re,collections
def load(f):
    S={'sec':None,'site':{},'lock':{},'comm':{},'t':collections.defaultdict(lambda:[0,0]),'s':{},'i915':{},'rx':0,'blank':''}
    for l in open(f,errors='replace'):
        l=l.rstrip('\r\n')
        if l.startswith('=== up'): S['up']=float(l.split()[2]); continue
        if l.startswith('=== '): S['sec']=l[4:]; continue
        sec=S['sec']; p=l.split()
        if sec=='rqhold':
            m=re.match(r'# lock (\S+) holds (\d+) ns_held (\d+) cont (\d+) ns_spun (\d+)',l)
            if m: S['lock'][m.group(1)]=tuple(map(int,m.groups()[1:]))
        elif sec=='i915' and len(p)==2 and p[1].isdigit(): S['i915'][p[0]]=int(p[1])
        elif sec=='netdev' and l.strip().startswith('wifi0:'): S['rx']=int(l.split(':',1)[1].split()[0])
        elif sec=='blank': S['blank']+=l.strip()+' '
        elif sec=='procs' and p and p[0]=='P': S['comm'][p[1]]=p[2]
        elif sec=='sysstat' and p:
            if p[0]=='t' and len(p)>=5: S['t'][(p[1],p[2])]=[int(p[3]),int(p[4])]
            elif p[0]=='s' and len(p)>=5: S['s'][(p[1],int(p[2]))]=(int(p[3]),int(p[4]))
    return S
def win(a,b,name):
    dt=b['up']-a['up']; out={}
    L=[(x-y)/dt for x,y in zip(b['lock'].get('TASKS',(0,)*4),a['lock'].get('TASKS',(0,)*4))]
    out['tasks']=L
    g=collections.defaultdict(lambda:[0,0])
    for k,v in b['t'].items():
        va=a['t'].get(k,[0,0]) if k in a['t'] else [0,0]
        g[k[1]][0]+=v[0]-va[0]; g[k[1]][1]+=v[1]-va[1]
    byname=collections.defaultdict(lambda:[0,0])
    for tg,(c,s) in g.items(): n=b['comm'].get(tg,a['comm'].get(tg,'?')); byname[n][0]+=c; byname[n][1]+=s
    tot=sum(v[0] for v in g.values()); sy=sum(v[1] for v in g.values())
    print(f"## {name} dt={dt:.1f}s blank=[{b['blank'][:40]}]")
    print(f"TASKS acq/s={L[0]:.0f} held_ms/s={L[1]/1e6:.1f} cont/s={L[2]:.0f} spin_ms/s={L[3]/1e6:.2f}")
    print(f"total CPU%={tot/dt/1e7:.0f} (sys {sy/dt/1e7:.0f}) flips/s={(b['i915'].get('flips',0)-a['i915'].get('flips',0))/dt:.1f} exec_vcs/s={(b['i915'].get('exec_vcs',0)-a['i915'].get('exec_vcs',0))/dt:.1f} wifiRX KB/s={(b['rx']-a['rx'])/dt/1e3:.0f}")
    print(' '.join(f"{n}={v[0]/dt/1e7:.1f}/{v[1]/dt/1e7:.1f}" for n,v in sorted(byname.items(),key=lambda x:-x[1][0])[:14]))
    for want in ('RDD_Process','firefox','cosmic-comp'):
        tgs=[t for t,n in b['comm'].items() if n==want]
        for tg in tgs:
            rows=[]
            for (t,nr),(c,ns) in b['s'].items():
                if t!=tg: continue
                ca,nsa=a['s'].get((t,nr),(0,0)); rows.append((nr,(c-ca)/dt,(ns-nsa)/dt/1e7))
            rows=sorted(rows,key=lambda r:-r[1])[:6]
            print(f"  sc {want}[{tg}] "+' '.join(f"nr{r[0]}:{r[1]:.0f}/s,{r[2]:.1f}%" for r in rows))
    return tot/dt/1e7,L
files=sys.argv[1:]
S=[load(f) for f in files]
for i in range(len(S)-1): win(S[i],S[i+1],f"{files[i].split('/')[-1]}->{files[i+1].split('/')[-1]}")
