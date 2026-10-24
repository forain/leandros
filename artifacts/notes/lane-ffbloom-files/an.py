import sys,re,collections
L=open(sys.argv[1]).read().split('\n'); sec={};cur=None
for l in L:
    if l.startswith('=='): cur=l[2:];sec[cur]=[];continue
    if cur: sec[cur].append(l)
names={}
for l in sec.get('PS',[]):
    w=l.split(None,1)
    if len(w)==2: names[w[0]]=w[1][:60]
if 'CM' in sec:
    for l in sec['CM']:
        w=l.split(None,1)
        if len(w)==2: names[w[0]]=w[1]+' '+names.get(w[0],'')[:30]
def ep(lines):
    d={}
    for l in lines:
        if not l or l[0]=='#' or '|' not in l: continue
        a,b=l.split('|',1); w=a.split()
        fds={}
        for t in b.split():
            f,n,k=t.split(':'); fds[(f,k)]=int(n)
        d[(w[0],w[1],w[2])]=(int(w[3]),int(w[4]),int(w[5]),fds)
    return d
e0,e1=ep(sec['E0']),ep(sec['E1'])
tot=collections.Counter(); rows=[]
for k,v in e1.items():
    o=e0.get(k,(0,0,0,{}))
    dp,du,dl=v[0]-o[0],v[1]-o[1],v[2]-o[2]
    fd={f:n-o[3].get(f,0) for f,n in v[3].items() if n-o[3].get(f,0)>0}
    tot[k[0]]+=dl
    rows.append((dl,k,dp,du,fd))
rows.sort(reverse=True)
want=sys.argv[2].split(',') if len(sys.argv)>2 else None
print("total useless/s", sum(tot.values())/10)
for dl,k,dp,du,fd in rows:
    if (want and k[0] not in want) and dl<20: continue
    print(f"{k[0]:>4} {names.get(k[0],'?')[:40]:40} ep{k[1]} parks={dp} useful={du} useless={dl} ({dl/10:.1f}/s) {sorted(fd.items(),key=lambda x:-x[1])[:5]}")
