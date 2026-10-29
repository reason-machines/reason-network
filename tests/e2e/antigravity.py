"""Native hook and sidecar contracts against an isolated real daemon, with an agentapi fixture.
Run: cargo build && python3 tests/e2e/antigravity.py
This verifies the documented adapter interface; it does not launch Google's App or a model.
"""
import json, os, pathlib, select, socket, subprocess, tempfile, time

ROOT = pathlib.Path(__file__).resolve().parents[2]
BIN = ROOT / 'target/debug/reason-network'

def main():
    with tempfile.TemporaryDirectory(prefix='rn-antigravity-', dir='/tmp') as directory:
        root = pathlib.Path(directory)
        project = root / 'project'; project.mkdir()
        state = root / 'network'; executables = root / 'bin'; executables.mkdir()
        log = root / 'calls.jsonl'; fail = root / 'refuse'
        env = {k:v for k,v in os.environ.items() if not k.startswith(('CLAUDE','CODEX','GEMINI','REASON_NETWORK','TINCAN'))}
        env.update(REASON_NETWORK_HOME=str(state),REASON_NETWORK_NO_SERVICE='1',REASON_NETWORK_NO_UPDATE='1',REASON_NETWORK_NO_TRAY='1',REASON_NETWORK_NO_MDNS='1',REASON_NETWORK_MACHINE='fixture',PATH=str(executables)+os.pathsep+env.get('PATH',''),ANTIGRAVITY_PROBE_LOG=str(log),ANTIGRAVITY_PROBE_FAIL=str(fail))
        api = executables / 'agentapi'
        api.write_text('#!/usr/bin/env python3\nimport os,sys,json\nif os.path.exists(os.environ["ANTIGRAVITY_PROBE_FAIL"]): sys.exit(1)\nwith open(os.environ["ANTIGRAVITY_PROBE_LOG"],"a") as f: print(json.dumps(sys.argv[1:]),file=f)\n')
        api.chmod(0o700)
        def cli(*args,payload=None):
            result=subprocess.run([str(BIN),*args],input=json.dumps(payload) if payload is not None else None,capture_output=True,text=True,env=env,cwd=project,timeout=30)
            assert result.returncode == 0, result.stderr
            return result.stdout
        def call(op,**fields):
            with socket.socket(socket.AF_UNIX) as connection:
                connection.settimeout(10);connection.connect(str(state/'network.sock'))
                connection.sendall((json.dumps(dict(id=1,op=op,**fields))+'\n').encode())
                response=json.loads(connection.makefile().readline())
                return response
        def wait(predicate,timeout=20):
            end=time.monotonic()+timeout
            while time.monotonic()<end:
                try: result=predicate()
                except (OSError,ValueError,KeyError): result=False
                if result:return result
                time.sleep(.1)
            raise AssertionError('condition did not become true')
        def hooks(sid,event,workspaces=None):
            return cli('hook','antigravity',event,payload=dict(conversationId=sid,workspacePaths=workspaces or [str(project)],transcriptPath=str(root/'.gemini/antigravity/brain'/sid/'transcript.jsonl'),invocationNum=0,initialNumSteps=0))
        def sessions():return call('who')['result']['sessions']
        def receipt(ids):return call('delivery_status',**{'from':'cli:sender','ids':ids})['result']
        def sent(to,body,key):
            response=call('send',**{'from':'cli:sender','to':to,'body':body,'idempotency_key':key})
            assert response['ok'],response
            return response['result']['ids']
        sidecar=mcp=None
        try:
            cli('init','fixture','--as','alice','--no-setup')
            call('hook',agent='cli',event='stop',session_id='sender',name='sender',cwd=str(project),pid=os.getpid(),mail=False,through_reason=False)
            first='b984f9b8-e5c6-44f0-85e6-9529cf7d4f77';second='3c4edbfc-5442-486d-a53e-faeecb3f892f'
            intro=json.loads(hooks(first,'user-prompt-submit'))
            assert first in intro['injectSteps'][0]['userMessage']
            recipient=next(s['address'] for s in sessions() if s['agent']=='antigravity')
            ids=sent(recipient,'Receive at the next invocation','hook-message')
            assert hooks(first,'post-tool-use') == ''
            assert receipt(ids)[0]['stage']=='queued'
            injected=json.loads(hooks(first,'user-prompt-submit'))
            assert 'Receive at the next invocation' in injected['injectSteps'][0]['userMessage']
            assert receipt(ids)[0]['stage']=='handed_to_agent'
            print('PASS native PreInvocation injects; PostTool leaves mail unread')
            hooks(second,'user-prompt-submit')
            mcp=subprocess.Popen([str(BIN),'mcp'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,env=dict(env,REASON_NETWORK_AGENT='antigravity'),cwd=project)
            def rpc(i,method,params):
                mcp.stdin.write(json.dumps(dict(jsonrpc='2.0',id=i,method=method,params=params))+'\n');mcp.stdin.flush()
                if not select.select([mcp.stdout],[],[],10)[0]:
                    mcp.terminate(); _,error=mcp.communicate(timeout=5)
                    raise AssertionError('MCP timeout: '+error)
                return json.loads(mcp.stdout.readline())
            rpc(1,'initialize',dict(protocolVersion='2025-06-18',capabilities={},clientInfo=dict(name='antigravity',version='fixture')))
            mcp.stdin.write(json.dumps(dict(jsonrpc='2.0',method='notifications/initialized'))+'\n');mcp.stdin.flush()
            for i,sid in enumerate([first,second],2):
                response=rpc(i,'tools/call',dict(name='list_team',arguments=dict(session_id=sid)))
                assert not response['result'].get('isError'),response
            rejected=rpc(4,'tools/call',dict(name='list_team',arguments=dict(session_id='unknown')))
            assert rejected['result']['isError']
            assert len([s for s in sessions() if s['agent']=='antigravity'])==2
            print('PASS concurrent chats bind to known native conversations; unknown identity is refused')
            sidecar=subprocess.Popen([str(BIN),'antigravity-sidecar'],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            wait(lambda:all(s['tier']=='push' for s in sessions() if s['agent']=='antigravity'))
            ids=sent(recipient,'Wake this idle conversation','idle-message')
            wait(lambda:receipt(ids)[0]['stage']=='handed_to_agent')
            assert sent(recipient,'Wake this idle conversation','idle-message')==ids
            assert not call('send',**{'from':'cli:sender','to':recipient,'body':'Different content','idempotency_key':'idle-message'})['ok']
            lines=[json.loads(line) for line in log.read_text().splitlines()]
            assert len(lines)==1 and lines[0][0:2]==['send-message',first] and ids[0] in lines[0][2],lines
            print('PASS idle sidecar hands off once; retry preserves id and different-content reuse is refused')
            fail.touch();ids=sent(recipient,'Retry the failed handoff','failure-message')
            wait(lambda:any(s['address']==recipient and s['tier']=='turn' for s in sessions()))
            assert next(s['unread'] for s in sessions() if s['address']==recipient)==1
            fail.unlink()
            wait(lambda:receipt(ids)[0]['stage']=='handed_to_agent',30)
            lines=[json.loads(line) for line in log.read_text().splitlines()]
            assert len(lines)==2 and ids[0] in lines[-1][2]
            print('PASS refused handoff remains unread and retries with the original message id')
            cli('stop')
            wait(lambda:len([s for s in sessions() if s['agent']=='antigravity' and s['tier']=='push'])==2)
            call('hook',agent='cli',event='stop',session_id='sender',name='sender',cwd=str(project),pid=os.getpid(),mail=False,through_reason=False)
            ids=sent(recipient,'Still reachable after a daemon restart','restart-message')
            wait(lambda:receipt(ids)[0]['stage']=='handed_to_agent')
            assert not rpc(5,'tools/call',dict(name='list_team',arguments=dict(session_id=first)))['result'].get('isError')
            print('PASS idle App bindings and MCP identity recover after a daemon restart without another hook')
            sidecar.terminate();sidecar.wait(timeout=5)
            sidecar=subprocess.Popen([str(BIN),'antigravity-sidecar'],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
            wait(lambda:len([s for s in sessions() if s['agent']=='antigravity' and s['tier']=='push'])==2)
            ids=sent(recipient,'Still reachable after a sidecar restart','sidecar-restart-message')
            wait(lambda:receipt(ids)[0]['stage']=='handed_to_agent')
            print('PASS sidecar restart preserves native conversation identity')
            cloud_command=executables/'reason'
            cloud_command.write_text('#!/bin/sh\nexit 0\n');cloud_command.chmod(0o700)
            registered=call('hook',agent='reason',event='stop',session_id='cloud-check',name='cloud-check',cwd=str(project),mail=False,through_reason=True)
            cloud=registered['result']['address']
            ids=sent(cloud,'Only a cloud server acknowledgment','cloud-acceptance')
            wait(lambda:next(s['unread'] for s in sessions() if s['address']==cloud)==0)
            assert receipt(ids)[0]['stage']=='accepted',receipt(ids)
            print('PASS Reason cloud server acceptance is not claimed as agent handoff')
            health=call('diagnostics')['result'];serialized=json.dumps(health)
            assert first not in serialized and str(project) not in serialized and 'Retry the failed handoff' not in serialized
            assert any(s['agent']=='antigravity' and s['adapterConnected'] for s in health['sessions'])
            print('PASS health reports capabilities without content, conversation ids or project paths')
            blocked=root/'disabled-project';blocked.mkdir()
            cli('folder',str(blocked),'off')
            assert hooks(first,'user-prompt-submit',[str(project),str(blocked)])==''
            cli('stop')
            wait(lambda:len([s for s in sessions() if s['agent']=='antigravity' and s['tier']=='push'])==1)
            assert not any(s['address']==recipient for s in sessions())
            print('PASS a hook with a disabled secondary workspace revokes its persistent App binding')
            cli('folder',str(blocked),'on')
            hooks(first,'user-prompt-submit',[str(project),str(blocked)])
            wait(lambda:len([s for s in sessions() if s['agent']=='antigravity' and s['tier']=='push'])==2)
            cli('folder',str(blocked),'off')
            wait(lambda:len([s for s in sessions() if s['agent']=='antigravity' and s['tier']=='push'])==1)
            assert not any(s['address']==recipient for s in sessions())
            print('PASS restoration revalidates all saved workspaces even without a new hook')
        finally:
            for process in [mcp,sidecar]:
                if process is not None:
                    process.terminate()
                    try:process.wait(timeout=5)
                    except subprocess.TimeoutExpired:process.kill();process.wait()
            cli('stop')

if __name__=='__main__': main()
