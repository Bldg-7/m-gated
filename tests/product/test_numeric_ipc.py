"""명시 합성 환경의 실제 loopback IPC만 검증한다. 제품/개인 환경은 읽지 않는다."""
import importlib.util
import json
import pathlib
import subprocess
import sys
import threading
import unittest
import urllib.error
from unittest import mock
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = pathlib.Path(__file__).resolve().parents[2]

class NumericIpcTests(unittest.TestCase):
    def setUp(self):
        self.requests = {'direct':0, 'trap':0}
        requests = self.requests
        class Trap(BaseHTTPRequestHandler):
            def do_GET(self):
                requests['trap'] += 1
                self.send_response(502); self.end_headers()
            def log_message(self, *args):
                pass
        self.trap = ThreadingHTTPServer(('127.0.0.1',0), Trap)
        trap_url = 'http://127.0.0.1:'+str(self.trap.server_port)
        class Direct(BaseHTTPRequestHandler):
            def do_GET(self):
                requests['direct'] += 1
                if self.path == '/redirect':
                    self.send_response(307); self.send_header('Location',trap_url+'/redirect-target'); self.end_headers()
                else:
                    self.send_response(200); self.send_header('Content-Length','2'); self.end_headers(); self.wfile.write(b'{}')
            def log_message(self, *args):
                pass
        self.direct = ThreadingHTTPServer(('127.0.0.1',0), Direct)
        self.jobs = []
        for server in [self.trap,self.direct]:
            job = threading.Thread(target=server.serve_forever,kwargs={'poll_interval':0.05});job.start();self.jobs.append(job)
        self.proxy = trap_url

    def tearDown(self):
        for server in [self.trap,self.direct]:
            server.shutdown();server.server_close()
        for job in self.jobs:
            job.join(timeout=2);self.assertFalse(job.is_alive())

    def child(self, path, operation):
        environment = {name:self.proxy for name in ['HTTP_PROXY','http_proxy','HTTPS_PROXY','https_proxy','ALL_PROXY','all_proxy']}
        environment.update({'NO_PROXY':'','no_proxy':''})
        result = subprocess.run([sys.executable,'-I',str(pathlib.Path(__file__).resolve()),'--child'],input=json.dumps({'url':'http://127.0.0.1:'+str(self.direct.server_port)+path,'operation':operation}).encode(),env=environment,capture_output=True,timeout=3)
        self.assertEqual(result.returncode,0,'명시 합성 환경 IPC child 실패')
        self.assertEqual(result.stderr,b'')
        return result.stdout

    def test_numeric_ipc_uses_no_ambient_proxy_in_actual_env_clear_child(self):
        self.assertEqual(self.child('/normal','direct'),b'NUMERIC_IPC_DIRECT_OK\n')
        self.assertEqual(self.requests,{'direct':1,'trap':0})

    def test_numeric_ipc_never_follows_loopback_redirect(self):
        self.assertEqual(self.child('/redirect','redirect'),b'NUMERIC_IPC_REDIRECT_REJECTED\n')
        self.assertEqual(self.requests,{'direct':1,'trap':0})

    def test_runner_explicitly_disables_ambient_proxy_and_redirects(self):
        source = (ROOT/'scripts/test-product.py').read_text()
        self.assertTrue('urllib.request.urlopen(' not in source, '기본 opener의 ambient proxy 경계를 제거해야 한다')
        self.assertTrue('numeric_opener()' in source)

def run_child():
    spec = importlib.util.spec_from_file_location('product_numeric_ipc',ROOT/'scripts/product_numeric_ipc.py')
    module = importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
    command = json.loads(sys.stdin.buffer.read(2048))
    try:
        # 개인 환경이 아닌 부모가 명시한 합성 proxy map만 있는 env_clear child다.
        # getproxies 호출 자체도 금지하여 환경을 조회하지 않음을 대조한다.
        with mock.patch('urllib.request.getproxies',side_effect=AssertionError('ambient proxy 조회 금지')):
            try:
                with module.numeric_opener().open(command['url'],timeout=1) as response:
                    assert command['operation']=='direct' and response.status==200 and response.read(3)==b'{}'
                print('NUMERIC_IPC_DIRECT_OK')
            except urllib.error.HTTPError as error:
                assert command['operation']=='redirect' and error.code==307
                print('NUMERIC_IPC_REDIRECT_REJECTED')
        return 0
    except Exception:
        print('NUMERIC_IPC_CHILD_FAILED')
        return 1

if __name__ == '__main__':
    if sys.argv[1:]==['--child']:
        sys.exit(run_child())
    unittest.main()
