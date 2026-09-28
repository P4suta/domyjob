# Unverified checkpoint draft; do not treat this as a passing test.
# Read ../remaining-work.md before running or adapting this script.
# Known issues: machine-specific paths, decimal event IDs, and outdated EventData JSON paths.
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time

root = Path(tempfile.mkdtemp(prefix='case-', dir='/private/tmp/domyjob-chat-e2e-adapter'))
bin_dir = root / 'bin'
bin_dir.mkdir()
work = root / 'work'
work.mkdir()
binary = Path('/Users/yasunobu/projects/github.com/P4suta/domyjob/target/debug/domyjob')
fake = bin_dir / 'codex'
fake.write_text('#!' + sys.executable + '\n' + r'''
import fcntl, json, os, sys, time
from pathlib import Path
prompt = sys.stdin.read()
args = sys.argv[1:]
record = {'args':args, 'prompt':prompt, 'agent':os.environ.get('DOMYJOB_CHAT_AGENT'), 'pid':os.getpid()}
lock = open('provider.lock', 'a')
try:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    record['overlap'] = True
with open('calls.jsonl', 'a') as output:
    output.write(json.dumps(record) + '\n')
if 'DELAY' in prompt:
    time.sleep(0.15)
if 'INCOMPLETE' in prompt:
    print(json.dumps({'type':'thread.started','thread_id':'session-test-1'}))
    print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':'partial'}}))
else:
    print(json.dumps({'type':'thread.started','thread_id':'session-test-1'}))
    print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':'fake complete answer'}}))
    print(json.dumps({'type':'turn.completed','usage':{'input_tokens':1,'output_tokens':1}}))
''')
fake.chmod(0o700)
notify = bin_dir / 'osascript'
notify.write_text('#!/bin/sh\nexit 0\n')
notify.chmod(0o700)
env = dict(os.environ, DOMYJOB_STATE=str(root / 'state'), PATH=str(bin_dir) + ':' + os.environ['PATH'], RUSTC_WRAPPER='')
env.pop('DOMYJOB_CHAT_AGENT', None)

def execute(args, *, expected=0, extra=None):
    selected = dict(env)
    selected.update(extra or {})
    result = subprocess.run([str(binary), *args], env=selected, cwd=work, text=True, capture_output=True, timeout=90)
    if result.returncode != expected:
        raise AssertionError({'args':args,'code':result.returncode,'stdout':result.stdout,'stderr':result.stderr})
    return json.loads(result.stdout)

def cli(*args, expected=0, extra=None):
    return execute(['chat', '--json', *args], expected=expected, extra=extra)

def calls():
    return [json.loads(line) for line in (work / 'calls.jsonl').read_text().splitlines()]

def mcp(name, arguments, extra=None):
    requests = [
        {'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2025-06-18','capabilities':{},'clientInfo':{'name':'local-e2e','version':'1'}}},
        {'jsonrpc':'2.0','method':'notifications/initialized'},
        {'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':name,'arguments':arguments}},
    ]
    selected = dict(env)
    selected.update(extra or {})
    result = subprocess.run([str(binary),'mcp'],input=''.join(json.dumps(item)+'\n' for item in requests), env=selected,cwd=work,text=True,capture_output=True,timeout=90)
    assert result.returncode == 0, result.stderr
    response = json.loads(result.stdout.splitlines()[-1])['result']
    assert not response['isError'], response
    return response['structuredContent']

print('isolated_test_root=' + str(root), flush=True)
cli('agent','start','reviewer','--kind','codex','--cwd',str(work))
first = cli('ask','reviewer','FIRST','--timeout','10')
assert first['state'] == 'answered', first
second = cli('ask','reviewer','SECOND','--timeout','10')
assert second['state'] == 'answered', second
records = calls()
assert len(records) == 2, records
assert 'resume' not in records[0]['args'], records[0]
assert records[1]['args'][-3:] == ['resume','session-test-1','-'], records[1]
assert len({first['message_id'],second['message_id']}) == 2
print('managed_first_and_resume=answered; provider_invocations=2', flush=True)
failed = cli('ask','reviewer','INCOMPLETE','--timeout','10',expected=1)
assert failed['state'] == 'failed', failed
assert len(calls()) == 3
print('incomplete_provider_result=failed; provider_invocations=3',flush=True)
cli('agent','attach','manual','--kind','codex','--cwd',str(work),'--session','manual-session')
pending = cli('ask','manual','MANUAL','--timeout','0',expected=3)
assert pending['state'] == 'pending', pending
inbox = mcp('chat_inbox',{}, {'DOMYJOB_CHAT_AGENT':'manual'})
assert pending['message_id'] in [event['origin']+':'+str(event['seq']) for event in inbox['events']], inbox
response = mcp('chat_reply',{'message':pending['message_id'],'text':'manual exact reply'}, {'DOMYJOB_CHAT_AGENT':'manual'})
assert response['event']['data']['details']['text'] == 'manual exact reply', response
owner_inbox = mcp('chat_inbox',{})
assert response['message_id'] in [event['origin']+':'+str(event['seq']) for event in owner_inbox['events']], owner_inbox
assert len(calls()) == 3
print('attached_mcp_inbox_and_reply=passed; provider_invocations_unchanged=3',flush=True)

def concurrent_ask(index):
    return cli('ask','reviewer',f'DELAY concurrent {index}','--timeout','20')
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    simultaneous = list(pool.map(concurrent_ask, range(6)))
assert all(item['state'] == 'answered' for item in simultaneous), simultaneous
assert len({item['message_id'] for item in simultaneous}) == 6
records = calls()
assert len(records) == 9, records
assert not any(record.get('overlap') for record in records), records
assert all(record['args'][-3:] == ['resume','session-test-1','-'] for record in records[1:]), records
print('simultaneous_asks=6 answered; total_provider_invocations=9; overlaps=0', flush=True)
thread = cli('thread','reviewer')
for item in [first,second,failed,*simultaneous]:
    request = item['message_id']
    resolutions = [event for event in thread['events'] if (event['data'].get('details',{}).get('mode',{}).get('details',{}).get('request') == request or event['data'].get('details',{}).get('request') == request)]
    assert len(resolutions) == 1, {'request':request,'resolutions':resolutions}
print('every_managed_request_has_exactly_one_durable_resolution=passed',flush=True)
print('E2E_OK',flush=True)
