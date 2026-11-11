#!/usr/bin/env python3
"""전용 합성 지원과 UID65532 제품을 별도 cgroup에서 실행한다. 기존 컨테이너는 변경하지 않는다."""
import json
import os
import pathlib
import re
import subprocess
import tempfile
import selectors
import threading
import time
import uuid
import urllib.request
import math
from product_numeric_ipc import numeric_opener

RPC_BUDGET_MS = 2000

ROOT = pathlib.Path(__file__).resolve().parents[1]
os.chdir(ROOT)
assert os.environ['TEST_DATABASE_URL'] == 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
case = os.environ.get('TEST_PRODUCT_CASE', 'smoke')
assert case in ['smoke', 'baseline', 'boundary', 'mixed', 'soak', 'fault', 'retention', 'cycles', 'all']
support_name = 'm-gated-full-v1-product-support'
product_name = 'm-gated-full-v1-product-runtime'
for name in [support_name, product_name]:
    assert subprocess.run(['docker', 'container', 'inspect', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode != 0, '기존 전용 이름도 임의 변경 금지'
subprocess.run(['docker', 'build', '-f', 'tests/product/Dockerfile', '-t', 'm-gated-full-v1-product-support', '.'], check=True)
support_image = subprocess.check_output(['docker','image','inspect','m-gated-full-v1-product-support','--format','{{.Id}}'],text=True).strip()
image = subprocess.check_output(['docker', 'image', 'inspect', 'm-gated-full-v1-product', '--format', '{{.Id}}/{{.Architecture}}'], text=True).strip()
try:
    cpu = subprocess.check_output(['sysctl', '-n', 'machdep.cpu.brand_string'], text=True).strip()
except (OSError, subprocess.CalledProcessError):
    cpu = subprocess.check_output(['uname', '-m'], text=True).strip()
expected = set(re.findall(r'=> "([a-z0-9_]+)"', (ROOT/'src/telemetry.rs').read_text().split('static VALUES:')[0]))
assert 0 < len(expected) < 100
owned = {}
def verify(identifier):
    value = json.loads(subprocess.check_output(['docker','container','inspect',identifier],text=True))[0]
    purpose, expected_image = owned[identifier]
    assert value['Id']==identifier and value['Image']==expected_image
    assert value['Config']['Labels']['app']=='m-gated' and value['Config']['Labels']['purpose']==purpose
    return value

def create(command, purpose, expected_image):
    identifier = subprocess.check_output(command,text=True).strip()
    assert re.fullmatch('[0-9a-f]{64}',identifier)
    owned[identifier]=(purpose,expected_image)
    verify(identifier)
    return identifier

def execute(identifier, args, **kwargs):
    verify(identifier)
    return subprocess.run(['docker','exec',identifier]+args,**kwargs)

def signal(identifier, name):
    verify(identifier)
    subprocess.run(['docker','kill','--signal='+name,identifier],check=True,stdout=subprocess.DEVNULL)

def remove(identifier):
    value=verify(identifier)
    assert not value['State']['Running']
    subprocess.run(['docker','rm',identifier],check=True,stdout=subprocess.DEVNULL)
    owned.pop(identifier)

(ROOT/'target/test-owned-schemas').mkdir(exist_ok=True, parents=True)
with tempfile.TemporaryDirectory(prefix='product-', dir=ROOT/'target') as temporary:
    directory = pathlib.Path(temporary)
    directory.chmod(0o755)
    support_command = ['docker', 'create', '--name', support_name, '--label', 'app=m-gated', '--label', 'purpose=S10-product-support', '--cpus=2', '--memory=1g', '--pids-limit=128', '-p', '127.0.0.1::64604/tcp', '-e', 'TEST_DATABASE_URL='+os.environ['TEST_DATABASE_URL'], '-v', str(directory)+':/fixture-data', '-v', str(ROOT/'target/test-owned-schemas')+':/fixture-ownership']
    for host in ['auth.example.test', 'api.anthropic.com', 'api.openai.com', 'api.deepseek.com']:
        support_command += ['--add-host',host+':127.0.0.1']
    support_id=create(support_command+['m-gated-full-v1-product-support'],'S10-product-support',support_image)
    support=subprocess.Popen(['docker','start','--attach',support_id],stdout=subprocess.PIPE,text=True)
    product = None
    reader = None
    observed = None
    def client(mode):
        wait_metric(observed, lambda m: observed['ipc_count'] > 0)
        execute(support_id, ['env','TEST_CPU_HOST='+cpu,'node', 'client.mjs', mode], check=True, timeout=3800 if mode=='soak' else 180)
    def control(path, body=None):
        expression = "fetch('http://127.0.0.1:64604"+path+"',{method:'POST',body:JSON.stringify("+json.dumps(body or {})+")}).then(async r=>{if(!r.ok)throw new Error('fixture control');await r.arrayBuffer()})"
        execute(support_id, ['node', '-e', expression], check=True, timeout=15)
    def wait_metric(state, predicate, timeout=12):
        with state['condition']:
            assert state['condition'].wait_for(lambda: state['latest'] is not None and predicate(state['latest']), timeout), '고정 telemetry barrier 미도달: '+json.dumps(state['latest'])
    def numeric_post(path, value):
        payload=json.dumps(value).encode()
        assert len(payload)<=16384
        request=urllib.request.Request('http://'+host_control+path, data=payload, headers={'Content-Type':'application/json'}, method='POST')
        started=time.monotonic()
        with numeric_opener().open(request, timeout=RPC_BUDGET_MS/1000) as response:
            assert response.status==200
            result=json.loads(response.read(1025))
            if path=='/metrics-snapshot':
                assert set(result)=={'barrier_generation'} and type(result['barrier_generation']) is int and result['barrier_generation']>=0
            else:
                assert result=={}
            elapsed_ms=(time.monotonic()-started)*1000
            assert elapsed_ms<=RPC_BUDGET_MS, '숫자 IPC whole-RPC 상한'
            return result, elapsed_ms
    def launch(extra=None, config='/fixture-data/config.yaml', maintenance=False):
        state = {'count': 0, 'latest': None, 'diagnostics': [], 'failure': None, 'condition': threading.Condition(), 'ipc_count':0, 'done':False, 'pending':None, 'barrier_generation':0, 'rpc_peak_ms':0, 'queue_age_peak_ms':0}
        epoch=str(uuid.uuid4())
        numeric_post('/metrics-epoch', {'epoch':epoch})
        env = {'DB_PASSWORD': 'synthetic-db-secret', 'CSRF_SIGNING_KEY': '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f', 'ANTHROPIC_API_KEY': 'synthetic-provider-secret', 'OPENAI_API_KEY': 'synthetic-openai-company-key', 'DEEPSEEK_API_KEY': 'synthetic-deepseek-company-key', 'TLS_CA_FILE': '/fixture-data/ca.pem'}
        env.update(extra or {})
        command = ['docker', 'create', '--name', product_name, '--label', 'app=m-gated', '--label', 'purpose=S10-product-runtime', '--network', 'container:'+support_id, '--read-only', '--cap-drop=ALL', '--security-opt=no-new-privileges', '--cpus=2', '--memory=1g', '--pids-limit=128', '--stop-timeout=35', '-v', str(directory)+':/fixture-data:ro']
        for key, value in env.items():
            command += ['-e', key+'='+value]
        if maintenance:
            command += ['-i']
        command += ['m-gated-full-v1-product', '--config', config]
        if maintenance:
            command += ['--maintenance']
        identifier=create(command,'S10-product-runtime',image.split('/')[0])
        child = subprocess.Popen(['docker','start','--attach']+(['--interactive'] if maintenance else [])+[identifier], stdin=subprocess.PIPE if maintenance else subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        child.owned_id=identifier
        def collect():
            try:
                for line in child.stderr:
                    assert len(line) <= 8192
                    assert not any(value in line for key, value in env.items() if key != 'TLS_CA_FILE')
                    assert 'synthetic prompt sentinel' not in line
                    if line.startswith('{"telemetry":'):
                        value = json.loads(line)
                        assert set(value)=={'telemetry'} and set(value['telemetry'])==expected
                        assert all(type(v) is int and v >= 0 for v in value['telemetry'].values())
                        with state['condition']:
                            state['count'] += 1
                            state['latest'] = value['telemetry']
                            state['pending'] = {'epoch':epoch,'sequence':state['count'],'telemetry':value['telemetry'],'observed_at':time.monotonic(),'barrier_generation':state['barrier_generation']}
                            state['condition'].notify_all()
                    else:
                        assert line in ['config_input_invalid: 기동 검증에 실패했습니다.\n', 'config_secret_invalid: 기동 검증에 실패했습니다.\n']
                        state['diagnostics'].append(line.split(':')[0])
                        assert len(state['diagnostics']) <= 1
            except Exception:
                state['failure'] = True
            finally:
                with state['condition']:
                    state['done']=True
                    state['condition'].notify_all()
        def forward_numeric():
            delivered=0
            try:
                while True:
                    with state['condition']:
                        state['condition'].wait_for(lambda: state['done'] or (state['pending'] is not None and state['pending']['sequence']>delivered))
                        value=state['pending']
                        if value is None or value['sequence']<=delivered:
                            break
                        value=dict(value)
                    queue_age_ms=math.ceil((time.monotonic()-value.pop('observed_at'))*1000)
                    age_ms=queue_age_ms+RPC_BUDGET_MS
                    assert 0<=queue_age_ms and age_ms<=15000
                    value['age_ms']=age_ms
                    acknowledged, elapsed_ms=numeric_post('/metrics-snapshot', value)
                    delivered=value['sequence']
                    with state['condition']:
                        state['rpc_peak_ms']=max(state['rpc_peak_ms'],elapsed_ms)
                        state['queue_age_peak_ms']=max(state['queue_age_peak_ms'],queue_age_ms)
                        state['barrier_generation']=acknowledged['barrier_generation']
                        state['ipc_count']=delivered
                        state['condition'].notify_all()
            except Exception:
                state['failure']=True
                print('S10_NUMERIC_IPC_FORWARD_FAILURE')
        child.metrics_sender=threading.Thread(target=forward_numeric)
        thread = threading.Thread(target=collect)
        child.metrics_sender.start();thread.start()
        return child, thread, state
    def finish(child, thread, state, success):
        child.wait(timeout=40)
        code = verify(child.owned_id)['State']['ExitCode']
        thread.join(timeout=3)
        child.metrics_sender.join(timeout=3)
        assert not thread.is_alive() and not child.metrics_sender.is_alive() and not state['failure']
        assert child.stdout.read() == ''
        assert code == (0 if success else 1)
        if success:
            assert state['count'] > 0 and not state['diagnostics']
            assert state['latest']['active_tasks']==0 and state['latest']['input_bytes']==0
            assert state['latest']['shutdown_remaining_milliseconds'] > 0
        remove(child.owned_id)
        return state
    try:
        selector=selectors.DefaultSelector();selector.register(support.stdout,selectors.EVENT_READ)
        try:
            assert selector.select(timeout=20),'지원 프로세스 ready event20초 상한'
            assert json.loads(support.stdout.readline(8193)) == {'event': 'PRODUCT_SUPPORT_READY'}
        finally:
            selector.close()
        host_control=subprocess.check_output(['docker','port',support_id,'64604/tcp'],text=True).strip()
        assert re.fullmatch(r'127\.0\.0\.1:[0-9]{1,5}',host_control)
        bindings=verify(support_id)['NetworkSettings']['Ports']['64604/tcp']
        assert len(bindings)==1 and bindings[0]['HostIp']=='127.0.0.1'
        assert bindings[0]['HostPort']==host_control.split(':')[1]
        product, reader, observed = launch(config='/missing-config')
        finish(product, reader, observed, False); product = None
        product, reader, observed = launch({'TLS_CA_FILE': '/fixture-data/missing.pem'})
        finish(product, reader, observed, False); product = None
        product, reader, observed = launch()
        client('smoke')
        signal(product.owned_id,'TERM')
        first = finish(product, reader, observed, True); product = None
        if case == 'retention':
            control('/retention-clock', {'at':'2025-03-01T00:00:00Z'})
        product, reader, observed = launch()
        if case != 'retention':
            client('restart')
        if case == 'retention':
            client('retention-prepare')
            signal(product.owned_id,'TERM')
            finish(product, reader, observed, True); product = None
            envelope=subprocess.check_output(['docker','exec',support_id,'node','-e',"fetch('http://127.0.0.1:64604/maintenance-envelope',{method:'POST'}).then(async r=>{if(!r.ok)throw new Error('fixture envelope');process.stdout.write(await r.text())})"],text=True,timeout=15)
            product, reader, observed = launch(maintenance=True)
            product.stdin.write(envelope);product.stdin.close();envelope=None
            assert product.wait(timeout=20)==0
            reader.join(timeout=3);product.metrics_sender.join(timeout=3);assert not reader.is_alive() and not product.metrics_sender.is_alive() and not observed['failure']
            response=json.loads(product.stdout.read(8193));assert response['result']['deleted_requests']==2
            remove(product.owned_id); product=None
            product, reader, observed = launch()
            client('retention-check')
            signal(product.owned_id,'TERM')
            finish(product, reader, observed, True); product=None
            product, reader, observed = launch()
            client('retention-check')
            print('S10_PRODUCT_POST_RETENTION_OS_RESTART_OK')
        elif case == 'fault':
            fault = subprocess.Popen(['docker','exec',support_id,'node','client.mjs','fault'], stdout=subprocess.PIPE, text=True)
            selector=selectors.DefaultSelector();selector.register(fault.stdout,selectors.EVENT_READ)
            try:
                assert selector.select(timeout=15), 'H2 response barrier timeout'
                assert fault.stdout.readline(8193).strip() == 'S10_FAULT_STREAM_OPEN'
            finally:
                selector.close()
            wait_metric(observed, lambda m: m['queue_waiters'] == 1 and 49152 <= m['queue_bytes'] <= 65536 and m['active_tasks'] == 1)
            control('/db-fail', {'enabled': True})
            wait_metric(observed, lambda m: m['ready'] == 0)
            client('fault-refused')
            signal(product.owned_id,'TERM')
            failed = finish(product, reader, observed, True); product = None
            assert fault.wait(timeout=10) == 0
            assert fault.stdout.read().strip() == 'S10_FAULT_CLIENT_RELEASED'
            assert failed['latest']['finalization_failed_total'] > 0
            assert failed['latest']['alarm_finalization_latch_total'] > 0
            assert failed['latest']['active_permits'] == 0 and failed['latest']['queue_bytes'] == 0
            control('/db-fail', {'enabled': False})
            control('/fault-expire')
            product, reader, observed = launch()
            wait_metric(observed, lambda m: m['unknown_requests'] == 1 and m['reaper_transitions_total'] >= 1)
            client('fault-recovered')
            print('S10_PRODUCT_DB_QUEUE_SIGTERM_RECOVERY_OK')
        else:
            for mode in (['baseline', 'boundary', 'mixed', 'soak'] if case=='all' else [] if case=='smoke' else [case]):
                client(mode)
        signal(product.owned_id,'TERM')
        final = finish(product, reader, observed, True); product = None
        print('S10_PRODUCT_IDENTITY '+json.dumps({'image': image, 'cpu_host': cpu, 'cpu_quota': 2, 'uid': 65532, 'read_only': True, 'cgroup_scope': 'product-only', 'first_samples': first['count'], 'last_samples': final['count'], 'numeric_rpc_peak_ms':final['rpc_peak_ms'], 'numeric_queue_age_peak_ms':final['queue_age_peak_ms'], 'numeric_rpc_bound_ms':RPC_BUDGET_MS}, ensure_ascii=False))
    finally:
        if product is not None:
            if verify(product.owned_id)['State']['Running']:
                signal(product.owned_id,'KILL')
            product.wait(timeout=15)
            if reader is not None:
                reader.join(timeout=3)
                product.metrics_sender.join(timeout=3)
        if support.poll() is None:
            execute(support_id,['node', '-e', "fetch('http://127.0.0.1:64604/shutdown',{method:'POST'}).then(r=>{if(!r.ok)process.exitCode=1})"], check=True, timeout=15)
            assert support.wait(timeout=15)==0
        for identifier in list(owned):
            remove(identifier)
assert not list((ROOT/'target/test-owned-schemas').iterdir())
