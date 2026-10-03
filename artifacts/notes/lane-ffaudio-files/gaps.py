import struct,math,sys
f=open(sys.argv[1],'rb').read(); d=f[44:]
n=len(d)//4; s=struct.unpack('<%dh'%(n*2), d[:n*4]); L=s[0::2]
r=44100; fw=r//100; a,b=float(sys.argv[2]),float(sys.argv[3])
holes=[w/100 for w in range(int(a*100), int(b*100)) if max(abs(v) for v in L[w*fw:(w+1)*fw])<300]
g=[]
for h in holes:
    if g and abs(h-g[-1][1])<0.015: g[-1][1]=h
    else: g.append([h,h])
print(len(g),"gaps, total",round(sum(y-x+0.01 for x,y in g),2),"s:",[(x,round(y-x+0.01,2)) for x,y in g])
