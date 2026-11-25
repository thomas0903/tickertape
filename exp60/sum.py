import json,sys,glob,os
for d in sys.argv[1:]:
    try: r=json.load(open(d+'/out/result.json'))
    except Exception as e:
        print(d,'NO RESULT',e); continue
    w=r['writer']; a=w['append_latency']; ro=w['roll_append_latency']; c=w.get('checkpoint_durable_latency') or {}
    s=[json.loads(l) for l in open(d+'/out/series.jsonl')]
    ser=[x['writer'] for x in s if 'writer' in x]
    n50=sum(1 for x in ser if (x.get('append_max_us') or 0)>=50000); n10=sum(1 for x in ser if (x.get('append_max_us') or 0)>=10000)
    rd=r['readers'][0]['latency'] if r['readers'] else {}
    f=lambda v: '-' if v is None else (f'{v:.0f}' if v>=10 else f'{v:.1f}')
    print(f"{os.path.basename(d):22s} late%={100*(w['late_records'] or 0)/max(1,w['count']):5.1f} dly={f(w.get('max_schedule_delay_us'))} app p99.9={f(a['p99_9_us'])} p99.99={f(a['p99_99_us'])} max={f(a['max_us'])} roll n={ro['count']} p50={f(ro['p50_us'])} p99={f(ro['p99_us'])} max={f(ro['max_us'])} dur p50={f(c.get('p50_us'))} max={f(c.get('max_us'))} rdr p99.9={f(rd.get('p99_9_us'))} max={f(rd.get('max_us'))} s>=10ms={n10} s>=50ms={n50} dirty={w['page_cache']['dirty_max_bytes']//2**20} wb={w['page_cache']['writeback_max_bytes']//2**20}")
