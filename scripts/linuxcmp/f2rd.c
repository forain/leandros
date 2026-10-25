/* f2rd DEV PATH: read one file from a LeandrOS f2fs volume, read-only,
 * without the kernel f2fs driver (layout as scripts/fsck-f2fs.py reads it:
 * single NAT/SIT copy, checkpoint journal overlay). Contents go to stdout. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <fcntl.h>
#include <unistd.h>
#define BS 4096
#define MAXJ 8192
static int fd;
static uint32_t jh[MAXJ]; static uint64_t jaddr[MAXJ]; static int nj;
static uint32_t r16(const uint8_t *b, int o){return b[o]|b[o+1]<<8;}
static uint32_t r32(const uint8_t *b, int o){return (uint32_t)b[o]|b[o+1]<<8|b[o+2]<<16|(uint32_t)b[o+3]<<24;}
static uint64_t r64(const uint8_t *b, int o){return r32(b,o)|(uint64_t)r32(b,o+4)<<32;}
static uint32_t crcle(uint32_t c,const uint8_t *p,size_t n){while(n--){c^=*p++;for(int k=0;k<8;k++)c=(c>>1)^(c&1?0xEDB88320u:0);}return c;}
static void die(const char *m){fprintf(stderr,"f2rd: %s\n",m);exit(1);}
static void raw(uint64_t n,uint8_t *b){memset(b,0,BS);if(pread(fd,b,BS,(off_t)(n*BS))<0)die("read");}
static void blk(uint64_t n,uint8_t *b){for(int i=0;i<nj;i++)if(jh[i]==n){raw(jaddr[i],b);return;}raw(n,b);}
static int cpok(const uint8_t *b){if(r32(b,164)==4092)return crcle(0xF2F52010u,b,4092)==r32(b,4092);return ~crcle(~0u,b,4092)==r32(b,4092);}
static uint32_t bps,nat_blk,main_blk,root,pack;
/* journal of pack at base; returns 0 ok (or none), -1 bad */
static int journal(const uint8_t *cp,uint64_t base){
  nj=0; if(r32(cp,3072)!=0x314A504Cu)return 0;
  uint32_t total=r32(cp,3076),j=r32(cp,3080),nx=r32(cp,3088);if(nx>64)nx=64;
  uint32_t tail=r32(cp,164)!=4092?0:(r32(cp,136)<=2?6:7);
  uint32_t cap=pack-1-tail,h=(j+1019)/1020;
  if(h+j!=total||total>cap+512*nx||j>MAXJ)return -1;
  uint32_t s=~0u; uint8_t b[BS]; uint32_t got=0;
  for(uint32_t k=0;k<total;k++){
    uint64_t a=k<cap?base+1+k:main_blk+(uint64_t)r32(cp,3092+4*((k-cap)/512))*bps+(k-cap)%512;
    raw(a,b); s=crcle(s,b,BS);
    if(k<h){if(r32(b,0)!=0x58494A4Cu)return -1;uint32_t c=r32(b,4);if(c>1020)c=1020;for(uint32_t q=0;q<c&&got<MAXJ;q++)jh[got++]=r32(b,16+4*q);}
    else if(k-h<MAXJ)jaddr[k-h]=a;
  }
  if(~s!=r32(cp,3084)||got!=j)return -1;
  nj=j;return 0;
}
static uint32_t nat(uint32_t nid){uint8_t b[BS];blk(nat_blk+nid/(BS/9),b);return r32(b,(nid%(BS/9))*9+5);}
static void node(uint32_t nid,uint8_t *b){uint32_t a=nat(nid);if(!a)die("no nat entry");blk(a,b);if(r32(b,4076)!=nid)die("node footer");}
/* logical block i of inode ib (direct addrs + dnode0/1 only) */
static uint32_t bmap(const uint8_t *ib,uint64_t i){
  uint32_t base=364; if(ib[3]&0x20){uint32_t x=r16(ib,364)/4;base+=(x>64?64:x)*4;}
  uint32_t n=(4056-base)/4;
  if(i<n)return r32(ib,base+4*i); i-=n;
  for(int d=0;d<2;d++){if(i<1019){uint32_t nid=r32(ib,4056+4*d);if(!nid)return 0;uint8_t b[BS];node(nid,b);return r32(b,4*i);}i-=1019;}
  die("file too large");return 0;
}
int main(int argc,char **argv){
  if(argc!=3)die("usage: f2rd DEV PATH");
  fd=open(argv[1],O_RDONLY);if(fd<0)die("open");
  uint8_t sb[BS],c0[BS],c1[BS],ib[BS],d[BS];
  raw(0,sb); const uint8_t *s=sb+1024;
  if(r32(s,0)!=0xF2F52010u)die("not f2fs");
  bps=1u<<r32(s,20); uint32_t cpb=r32(s,76); nat_blk=r32(s,84); main_blk=r32(s,92); root=r32(s,96);
  pack=(r32(s,52)/2)*bps;
  raw(cpb,c0); raw(cpb+pack,c1);
  int ok0=cpok(c0),ok1=cpok(c1),p1;
  if(ok0&&ok1)p1=r64(c1,0)>r64(c0,0);else if(ok0||ok1)p1=ok1;else p1=r64(c1,0)>r64(c0,0);
  if(journal(p1?c1:c0,cpb+(p1?pack:0))<0){p1=!p1;if(journal(p1?c1:c0,cpb+(p1?pack:0))<0)die("journal");}
  uint32_t ino=root; char *path=strdup(argv[2]);
  for(char *comp=strtok(path,"/");comp;comp=strtok(NULL,"/")){
    node(ino,ib); if((r16(ib,0)&0170000)!=0040000)die("not a dir");
    uint64_t nb=(r64(ib,16)+BS-1)/BS; uint32_t found=0; size_t cl=strlen(comp);
    for(uint64_t i=0;i<nb&&!found;i++){uint32_t a=bmap(ib,i);if(!a||a==0xFFFFFFFFu)continue;blk(a,d);
      for(int sl=0;sl<214;){if(!((d[sl/8]>>(sl%8))&1)){sl++;continue;}int e=30+sl*11;uint32_t nl=r16(d,e+8);
        if(nl==cl&&2384+sl*8+nl<=BS&&!memcmp(d+2384+sl*8,comp,cl)){found=r32(d,e+4);break;}
        sl+=nl?(nl+7)/8:1;}}
    if(!found)die("not found");ino=found;
  }
  node(ino,ib); uint64_t sz=r64(ib,16); if(sz>1<<20)die("too big");
  for(uint64_t i=0;i*BS<sz;i++){uint32_t a=bmap(ib,i);if(!a||a==0xFFFFFFFFu)memset(d,0,BS);else blk(a,d);
    size_t n=sz-i*BS>BS?BS:sz-i*BS; if(write(1,d,n)!=(ssize_t)n)die("write");}
  return 0;
}
