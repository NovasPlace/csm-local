"""Isolated real-host idle-wake check. No installed settings or persistent model chat is changed."""
import argparse, hashlib, json, os, queue, re, shutil, sqlite3, subprocess, threading, time, uuid
from pathlib import Path

def main():
    ap=argparse.ArgumentParser();ap.add_argument('--binary',type=Path,required=True);ap.add_argument('--output',type=Path,required=True)
    ap.add_argument('--claude',default=shutil.which('claude') or 'claude');args=ap.parse_args()
    binary=args.binary.resolve();root=args.output.resolve();root.mkdir(parents=True,exist_ok=False)
    project=root/'project';project.mkdir();db=root/'test.db';hidden=subprocess.CREATE_NO_WINDOW if os.name=='nt' else 0
    env=os.environ.copy()
    for key in ['CSM_DB','CSM_HOME','CLAUDE_PROJECT_DIR','CLAUDECODE','CODEX_APP_TOOLS_PIPE_PATH']:
        env.pop(key,None)
    env['CSM_HOME']=str(root/'data')
    def csm(words,payload=None):
        r=subprocess.run([str(binary),'--db',str(db),*words],cwd=project,env=env,input=None if payload is None else json.dumps(payload),
            text=True,encoding='utf-8',capture_output=True,timeout=30,creationflags=hidden)
        if r.returncode:raise RuntimeError(r.stderr[:300])
        return json.loads(r.stdout) if r.stdout.strip() else None
    sender_id=str(uuid.uuid4());receiver_id=str(uuid.uuid4());sender='codex:'+sender_id;receiver='claude:'+receiver_id
    csm(['hook','codex','--messaging'],{'hook_event_name':'SessionStart','source':'startup','cwd':str(project),'session_id':sender_id})
    with sqlite3.connect(db) as conn:conn.execute('UPDATE csm_wake_config SET enabled=1 WHERE singleton=1')
    normal=f'"{binary.as_posix()}" hook claude --db "{db.as_posix()}" --messaging'
    watch=f'"{binary.as_posix()}" wake-hook claude --db "{db.as_posix()}" --timeout-seconds 120'
    settings={'hooks':{event:[{'hooks':[{'type':'command','command':normal,'timeout':5}]}]
        for event in ['SessionStart','PostToolUse','Stop','SessionEnd']},'permissions':{'allow':['mcp__csm__*']}}
    settings['hooks']['Stop'][0]['hooks'].append({'type':'command','command':watch,'asyncRewake':True,'timeout':125})
    (root/'settings.json').write_text(json.dumps(settings));(root/'mcp.json').write_text(json.dumps({'mcpServers':{'csm':{
        'command':str(binary),'args':['--db',str(db),'mcp','--project',str(project)]}}}))
    prompt=('This is a bounded CSM native idle-wake integration test authorized by Donovan. Connect once to CSM using your own exact identity and private binding token from the startup hook. '
        'Then output READY_AND_STOP and finish that turn immediately. Do not call csm_wait or poll or use a background tool. '
        'If a later automatic CSM wake reminder arrives, read csm_inbox, acknowledge the test message you read, '
        'and send exactly one reply with content ECHO followed by a space and the challenge text, reply_to its ID, request_id idle-echo. '
        'The challenge starts CSM-IDLE-. Only then output WAKE_HANDLED and stop. Do not do other work or send other messages. Never reveal your binding token.')
    command=[args.claude,'--print','--verbose','--input-format','stream-json','--output-format','stream-json','--include-hook-events',
        '--no-session-persistence','--session-id',receiver_id,'--setting-sources','','--settings',str(root/'settings.json'),
        '--strict-mcp-config','--mcp-config',str(root/'mcp.json'),'--tools','','--allowedTools','mcp__csm__*']
    child=subprocess.Popen(command,cwd=project,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,
        text=True,encoding='utf-8',errors='replace',creationflags=hidden)
    child.stdin.write(json.dumps({'type':'user','message':{'role':'user','content':prompt}})+'\n');child.stdin.flush()
    lines=queue.Queue()
    def pump(stream,kind):
        for line in stream:lines.put((kind,line))
    for stream,kind in [(child.stdout,'stdout'),(child.stderr,'stderr')]:threading.Thread(target=pump,args=(stream,kind),daemon=True).start()
    events=[];first_result=False;sent=None;started=time.monotonic();idle_at=None;challenge='CSM-IDLE-'+uuid.uuid4().hex[:12];echo=None;host_error=None
    deadline=started+150
    while time.monotonic()<deadline:
        try:kind,line=lines.get(timeout=.2)
        except queue.Empty:kind=line=None
        if line:
            safe=re.sub(r'(?<![0-9a-f])[0-9a-f]{48}(?![0-9a-f])','[PRIVATE_BINDING_TOKEN]',line)
            events.append({'stream':kind,'line':safe.rstrip()})
            if kind=='stdout':
                try:e=json.loads(line)
                except json.JSONDecodeError:e={}
                if e.get('type')=='result':
                    if e.get('is_error'):host_error=e.get('result','host error')[:300]
                    if not first_result:first_result=True;idle_at=time.monotonic()
                if e.get('is_api_error_message'):host_error=str(e.get('error','host error'))[:300]
        if first_result and sent is None and time.monotonic()-idle_at>=1:
            with sqlite3.connect(db) as conn:
                state=conn.execute('SELECT state FROM csm_peers WHERE id=?',(receiver,)).fetchone()
                watcher=conn.execute('SELECT 1 FROM csm_wake_watchers WHERE peer_id=? AND expires_at>?',(receiver,int(time.time()*1000))).fetchone()
            if state and state[0]=='idle' and watcher and child.poll() is None:
                sent=csm(['send','--from',sender,'--to',receiver,'--request-id','idle-challenge',challenge])['message']
        if sent:
            replies=csm(['inbox','--peer',sender])['messages']
            echo=next((m for m in replies if m['content']=='ECHO '+challenge),None)
            if echo:break
        if child.poll() is not None and lines.empty():break
        if host_error:break
    if child.stdin and not child.stdin.closed:
        try:child.stdin.close()
        except OSError:pass
    try:child.wait(timeout=5)
    except subprocess.TimeoutExpired:child.kill();child.wait(timeout=5)
    status=csm(['message-status','--peer',sender,str(sent['id'])]) if sent else None
    with sqlite3.connect(db) as conn:
        accepted=conn.execute("SELECT count(*) FROM csm_wake_jobs WHERE state='accepted'").fetchone()[0]
        conn.execute('UPDATE csm_wake_config SET enabled=0 WHERE singleton=1')
    if echo:csm(['ack','--peer',sender,str(echo['id'])])
    report={'binary_sha256':hashlib.sha256(binary.read_bytes()).hexdigest(),'native_host':'Claude Code persistent stream-json process',
        'first_turn_completed_before_challenge':first_result,'challenge_sent_only_after_confirmed_idle_and_armed_watcher':sent is not None,
        'native_async_wake_accepted':accepted>0,'explicit_acknowledgment':bool(status and status['status']=='acknowledged'),
        'exact_reply_received':echo is not None,'no_new_user_message_after_first_turn':True,'host_error':host_error,'elapsed_seconds':round(time.monotonic()-started,2)}
    report['passed']=all(report[k] for k in ['first_turn_completed_before_challenge','challenge_sent_only_after_confirmed_idle_and_armed_watcher',
        'native_async_wake_accepted','explicit_acknowledgment','exact_reply_received']) and not host_error
    (root/'events.sanitized.json').write_text(json.dumps(events,indent=2));(root/'report.json').write_text(json.dumps(report,indent=2))
    print(json.dumps(report,indent=2));return 0 if report['passed'] else 1
if __name__=='__main__':raise SystemExit(main())
