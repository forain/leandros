#!/usr/bin/env python3
"""f2fs-lx-corrupt.py IMG ssa|sit — corrupt one SSA data entry (segment 1) or the SIT type of the warm node curseg of an lx image, to check that fsck-f2fs.py and fsck.f2fs flag it and f2fs-lx-ssa-rebuild.py repairs it."""
# usage: corrupt.py IMG ssa|sit
f=open(sys.argv[1],'r+b'); sb=1024
rd=lambda n:(f.seek(n*4096),bytearray(f.read(4096)))[1]
g=lambda o:struct.unpack_from('<I',rd(0),sb+o)[0]
sit,ssa,main=g(80),g(88),g(92)
if sys.argv[2]=='ssa':
    b=rd(ssa+1); struct.pack_into('<I',b,7*3,0x1234); f.seek((ssa+1)*4096); f.write(b)   # seg1 entry 3 (data seg) nid
else:
    cpb=g(76); c0=rd(cpb); c1=rd(cpb+512)
    cp=c1 if struct.unpack_from('<Q',c1,0)[0]>struct.unpack_from('<Q',c0,0)[0] else c0
    seg=struct.unpack_from('<I',cp,40)[0]   # node curseg 1 (WARM_NODE, SIT type 4)
    b=rd(sit+seg//55); o=(seg%55)*74; v=struct.unpack_from('<H',b,o)[0]; struct.pack_into('<H',b,o,(v&0x3ff)|(0<<10)); f.seek((sit+seg//55)*4096); f.write(b)
