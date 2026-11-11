#!/usr/bin/env bash
# 선행 build가 만든 Vite bundle과 최신 Rust의 실제 TLS/main 수명 시험이다.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${TEST_DATABASE_URL:?전용 TEST_DATABASE_URL 필요}"
test "$TEST_DATABASE_URL" = 'postgres://m_gated_test@127.0.0.1:64603/m_gated_test'
# 이 이미지는 Node/합성 TLS/PG 시험 전용이며 S10의 non-root 제품 이미지가 아니다.
docker build --label app=m-gated --label purpose=s3-process-test -f tests/process/Dockerfile -t m-gated-s3-main-test .
mkdir -p target/test-owned-schemas
# 실행 기한 초과 시 정확한 자기 label을 확인한 컨테이너만 정리한다.
python3 - <<'PY'
import os
import pathlib
import subprocess

name = 'm-gated-full-v1-s3-main'
command = ['docker', 'run', '--rm', '--name', name,
           '--label', 'app=m-gated', '--label', 'purpose=S3-actual-main-test',
           '--cpus=2', '--memory=512m', '--pids-limit=128', '--stop-timeout=35']
for host in ['auth.example.test', 'api.anthropic.com', 'api.openai.com', 'api.deepseek.com']:
    command += ['--add-host', host + ':127.0.0.1']
case = os.environ.get('TEST_S10_LOAD_CASE', '')
assert case in ['', 'all', 'baseline', 'boundary', 'mixed', 'soak']
command += ['-e', 'TEST_S10_LOAD_CASE=' + case]
command += ['-v', str(pathlib.Path('target/test-owned-schemas').resolve()) + ':/fixture-ownership',
            '-e', 'TEST_DATABASE_URL=' + os.environ['TEST_DATABASE_URL'], 'm-gated-s3-main-test']
try:
    result = subprocess.run(command, timeout=4200 if case in ['all', 'soak'] else 360 if case else 180)
    raise SystemExit(result.returncode)
except subprocess.TimeoutExpired:
    print('S3_RUNTIME_FIXTURE_TIMEOUT', flush=True)
    check = subprocess.run(['docker', 'inspect', '--format',
                            '{{index .Config.Labels "app"}}/{{index .Config.Labels "purpose"}}', name],
                           capture_output=True, text=True, timeout=10)
    if check.returncode == 0 and check.stdout.strip() == 'm-gated/S3-actual-main-test':
        subprocess.run(['docker', 'rm', '--force', name], check=True, timeout=15)
    raise SystemExit(124)
PY
docker image inspect m-gated-s3-main-test --format 'S3_TEST_IMAGE_ID={{.Id}}'
