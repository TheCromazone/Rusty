# Offline fixed-input CPU and peak-RSS comparison; no cloud credentials.
import os,sys,time,json,tempfile,statistics,subprocess,hashlib
from pathlib import Path
root=Path.cwd()

def measure(cmd, data=None):
    with tempfile.TemporaryFile() as out, tempfile.TemporaryFile() as err, tempfile.TemporaryFile() as inp:
        if data: inp.write(data);inp.seek(0)
        started=time.monotonic()
        p=subprocess.Popen(cmd, stdin=inp, stdout=out,stderr=err,env={**os.environ,'RUSTY_NO_DOTENV':'1'},cwd=root)
        _,status,r=os.wait4(p.pid,0);p.returncode=os.waitstatus_to_exitcode(status)
        out.seek(0); result=out.read();err.seek(0)
        if p.returncode: raise RuntimeError(err.read().decode())
        return {'wall_ms':(time.monotonic()-started)*1000,'cpu_ms':(r.ru_utime+r.ru_stime)*1000,'rss_bytes':r.ru_maxrss * (1024 if sys.platform.startswith('linux') else 1),'output':result.decode()}

def main():
    label, binary,cloud = sys.argv[1:]
    binary,cloud = str(Path(binary).resolve()),str(Path(cloud).resolve())
    fixture=root/'large-lines.txt'
    with fixture.open('wb') as f:
        for _ in range(1024):f.write(b'abc\n'*16384)
    data=json.dumps({'op':'execute','name':'read_file','args':{'path':str(fixture),'offset':1000000,'limit':20},'policy':{'mode':'read-only'},'approval':'Allow'}).encode()
    report={}
    for name,cmd,input_data in [('cloud_64MiB',[cloud],None),('read_64MiB',[binary,'--tool-rpc'],data)]:
        samples=[measure(cmd,input_data) for _ in range(6)]
        outputs=[s.pop('output') for s in samples]
        if name.startswith('cloud'):
            receipts=[json.loads(o) for o in outputs]
            assert len({r['tar_sha256'] for r in receipts})==1
            report['cloud_receipt']=receipts[0]
        else:
            assert all(json.loads(o)['ok'] for o in outputs),outputs[0]
            report['read_output_sha256']=hashlib.sha256(outputs[0].encode()).hexdigest()
        report[name]={k:statistics.median(s[k] for s in samples[1:]) for k in samples[0]}
    (root/f'focused-{label}.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report,indent=2))
if __name__=='__main__':main()
