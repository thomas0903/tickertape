# ana.py RUNDIR [minus]: group the writer main thread's long waits by stack; show timeline around the worst stalls
import sys,re,collections
d=sys.argv[1]; thr=float(sys.argv[2]) if len(sys.argv)>2 else 20000
ev=[];cur=None
for l in open(d+'/offcpu.txt'):
    m=re.match(r'^(\d+\.\d+) offcpu tid=(\d+) comm=(.+?) state=(\S+) us=(\d+)',l)
    if m: cur=[float(m[1]),int(m[2]),m[3],m[4],int(m[5]),[]]; ev.append(cur)
    elif cur is not None and l.startswith((' ','\t')) and l.strip(): cur[5].append(re.sub(r'\+\d+.*','',l.strip()))
    elif l.startswith('@'): cur=None
def key(e): return (e[2],e[3],' < '.join(x for x in e[5] if x not in('__schedule','schedule','io_schedule','perf_trace_sched_switch','bit_wait_io') )[:420])
for who in (lambda e:e[2].startswith('tt-sustained'),lambda e:not e[2].startswith('tt-')):
    g=collections.defaultdict(list)
    for e in ev:
        if e[2].startswith('tt-sustained') and not who(e): continue
        if who(e) and 'futex' not in ' '.join(e[5]): g[key(e)].append(e[4])
    for k,v in sorted(g.items(),key=lambda x:-sum(x[1]))[:9]:
        print(f"n={len(v)} total={sum(v)/1e6:.2f}s max={max(v)/1e3:.1f}ms n>={thr/1e3:.0f}ms={sum(1 for x in v if x>=thr)} | {k[0]} {k[1]} | {k[2]}")
    print()
