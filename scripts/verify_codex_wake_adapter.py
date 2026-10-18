"""Windows named-pipe contract checks; the fake host never invokes a model."""
import argparse,ctypes,json,os,struct,subprocess,tempfile,threading,uuid
from pathlib import Path
import msvcrt

kernel=ctypes.WinDLL('kernel32',use_last_error=True)
kernel.CreateNamedPipeW.argtypes=[ctypes.c_wchar_p,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_ulong,ctypes.c_void_p]
kernel.CreateNamedPipeW.restype=ctypes.c_void_p
kernel.ConnectNamedPipe.argtypes=[ctypes.c_void_p,ctypes.c_void_p];kernel.ConnectNamedPipe.restype=ctypes.c_int
def exact(f,n):
    data=b''
    while len(data)<n:
        chunk=f.read(n-len(data))
        if not chunk:raise EOFError('pipe closed')
        data+=chunk
    return data
def main():
    ap=argparse.ArgumentParser();ap.add_argument('--binary',type=Path,required=True);ap.add_argument('--output',type=Path,required=True);args=ap.parse_args()
    binary=args.binary.resolve();out=args.output.resolve();out.mkdir(parents=True,exist_ok=False);results=[]
    for case in ['idle','busy','missing_tool','oversized','lost_send_receipt']:
        pipe=r'\\.\pipe\csm-wake-qa-'+uuid.uuid4().hex
        handle=kernel.CreateNamedPipeW(pipe,3,0,1,65536,65536,0,None)
        if handle==ctypes.c_void_p(-1).value:raise ctypes.WinError(ctypes.get_last_error())
        calls=[];errors=[]
        def serve():
            try:
                ok=kernel.ConnectNamedPipe(handle,None)
                if not ok and ctypes.get_last_error()!=535:raise ctypes.WinError(ctypes.get_last_error())
                with os.fdopen(msvcrt.open_osfhandle(handle,os.O_RDWR|os.O_BINARY),'r+b',buffering=0) as f:
                    count={'busy':2,'missing_tool':1,'oversized':1}.get(case,3)
                    for _ in range(count):
                        n=struct.unpack('<I',exact(f,4))[0];q=json.loads(exact(f,n));calls.append(q)
                        if case=='oversized':f.write(struct.pack('<I',8*1024*1024+1));break
                        if q['method']=='tools/list':
                            result={'tools':[] if case=='missing_tool' else [{'name':name,'namespace':'codex_app'}for name in ['read_thread','send_message_to_thread']]}
                        else:
                            tool=q['params']['tool']
                            if tool=='read_thread':text=json.dumps({'thread':{'status':{'type':'active' if case=='busy' else 'idle'}}})
                            else:
                                if case=='lost_send_receipt':break
                                text='Wake notification sent'
                            result={'success':True,'contentItems':[{'type':'inputText','text':text}]}
                        body=json.dumps({'jsonrpc':'2.0','id':q['id'],'result':result}).encode();f.write(struct.pack('<I',len(body))+body)
            except Exception as e:errors.append(str(e))
        worker=threading.Thread(target=serve,daemon=True);worker.start()
        request={'pipe':pipe,'session_id':'qa-session','prompt':'CSM automatic inbox wake; read inbox, preserve authorization boundaries.'}
        run=subprocess.run([str(binary),'--db',str(out/'test.db'),'wake','codex-call'],input=json.dumps(request),text=True,
            encoding='utf-8',capture_output=True,timeout=30,creationflags=subprocess.CREATE_NO_WINDOW)
        worker.join(timeout=5);r=json.loads(run.stdout)
        expected={'idle':'accepted','busy':'busy','missing_tool':'failed','oversized':'failed','lost_send_receipt':'uncertain'}[case]
        passed=r['status']==expected and not errors
        if case=='busy':passed=passed and all(q.get('params',{}).get('tool')!='send_message_to_thread' for q in calls)
        if case=='idle':passed=passed and calls[-1]['params']['arguments']['threadId']=='qa-session'
        results.append({'case':case,'expected':expected,'actual':r['status'],'requests':len(calls),'passed':passed,'errors':errors})
    report={'passed':all(r['passed'] for r in results),'cases':results,'models_invoked':False}
    (out/'report.json').write_text(json.dumps(report,indent=2));print(json.dumps(report,indent=2));return 0 if report['passed'] else 1
if __name__=='__main__':raise SystemExit(main())
