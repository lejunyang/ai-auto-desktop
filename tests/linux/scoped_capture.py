"""Only owned Qt windows; run via verify-scoped-capture.ps1 on an exclusive desktop."""
import argparse
import base64
import hashlib
import json
import os
import pathlib
import selectors
import subprocess
import time
import uuid

parser = argparse.ArgumentParser()
for name in ('aad', 'helper', 'capture-helper', 'fixture', 'output'):
    parser.add_argument('--' + name, required=True)
args = parser.parse_args()
out = pathlib.Path(args.output)
out.mkdir(parents=True, exist_ok=True)
env = os.environ | {'AAD_LINUX_XTEST_HELPER': args.helper, 'AAD_LINUX_CAPTURE_HELPER': args.capture_helper,
                    'QT_ACCESSIBILITY': '1', 'QT_LINUX_ACCESSIBILITY_ALWAYS_ON': '1'}


def line(stream, timeout):
    with selectors.DefaultSelector() as selector:
        selector.register(stream, selectors.EVENT_READ)
        if not selector.select(timeout):
            raise RuntimeError('owned fixture/control response timed out')
    value = stream.readline()
    if not value:
        raise RuntimeError('owned fixture/control exited unexpectedly')
    return value


class Client:
    def __init__(self, mode):
        self.sid = None
        self.stderr = open(out / (mode + '-control.stderr'), 'w')
        self.log = open(out / (mode + '.ndjson'), 'w')
        self.p = subprocess.Popen([args.aad, 'control', 'serve'], env=env,
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True)

    def call(self, method, **values):
        request = {'request_id': str(uuid.uuid4()), 'session_id': self.sid, 'timeout_ms': 12000,
                   'operation': {'method': method, **values}}
        self.p.stdin.write(json.dumps(request) + '\n')
        self.p.stdin.flush()
        result = json.loads(line(self.p.stdout, 20))
        self.sid = result['session_id']
        if method != 'targets':
            self.log.write(json.dumps({'request': request, 'response': result}, ensure_ascii=False) + '\n')
            self.log.flush()
        return result

    def data(self, method, **values):
        result = self.call(method, **values)
        assert not result.get('error'), result
        return result['data']

    def close(self):
        try:
            if self.p.poll() is None:
                self.call('close')
        finally:
            self.p.stdin.close()
            try:
                self.p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.p.kill()
                self.p.wait()
            self.log.close()
            self.stderr.close()


rows = []
passed = False
try:
    for mode in ('clear', 'overlay', 'protected', 'protected-outside'):
        c = None
        stderr = open(out / (mode + '-fixture.stderr'), 'w')
        p = subprocess.Popen([args.fixture, mode], env=env, stdout=subprocess.PIPE, stderr=stderr, text=True)
        try:
            assert line(p.stdout, 10).strip() == 'READY'
            c = Client(mode)
            c.data('capabilities')
            for _ in range(30):
                owned = [r for r in c.data('targets')['resources'] if r.get('metadata', {}).get('process_id') == p.pid]
                if len(owned) == 1:
                    break
                time.sleep(.1)
            assert len(owned) == 1, 'owned application must resolve uniquely'
            rid = owned[0]['resource_id']
            observed = c.data('observe', resource_id=rid, max_nodes=100, view={'mode': 'nodes'})
            assert not observed['truncated']
            surface = next(n for n in observed['nodes'] if n['name'] == 'Capture surface')
            scoped = c.data('observe', resource_id=rid, max_nodes=100,
                            view={'mode': 'nodes', 'scope_target': surface['target']})
            assert not scoped['truncated']
            assert not any(n['name'] in ('Covering sibling', 'Protected sibling') for n in scoped['nodes'])
            surface = next(n for n in scoped['nodes'] if n['name'] == 'Capture surface')
            response = c.call('capture', target=surface['target'])
            row = {'case': mode, 'scope_nodes': len(scoped['nodes']), 'scope_excludes_sibling': True}
            if mode in ('overlay', 'protected'):
                error = response.get('error')
                assert error, 'scope omitted the covering sibling and capture incorrectly succeeded'
                expected = 'PROTECTED_ELEMENT' if mode == 'protected' else 'ACTION_UNSUPPORTED'
                assert error['code'].endswith(expected), response
                assert not (response.get('data') or {}).get('data'), 'a refused capture returned pixels'
                row.update(refused=error['code'], details=error.get('details'))
            else:
                assert not response.get('error'), response
                shot = response['data']
                data = base64.b64decode(shot['data'])
                image_path = out / (mode + '.png')
                image_path.write_bytes(data)
                pixels = json.loads(subprocess.run([args.fixture, '--verify-png', str(image_path)],
                    capture_output=True, text=True, timeout=5, check=True).stdout)
                assert (pixels['width'], pixels['height']) == (surface['bounds']['width'], surface['bounds']['height'])
                ratio = pixels['green_fraction']
                assert ratio > .9, 'independent pixel readback does not match the owned green surface'
                row.update(pixels)
            # Capturing must preserve the caller's original scope token/authority.
            found = c.data('find', observation_id=scoped['observation_id'], locator={'name': 'Capture surface'})
            assert found, 'capture consumed the original scoped observation'
            row['passed'] = True
            rows.append(row)
            print('PASS: ' + mode, flush=True)
        finally:
            try:
                if c:
                    c.close()
            finally:
                if p.poll() is None:
                    p.terminate()
                try:
                    p.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    p.kill()
                    p.wait()
                stderr.close()
    passed = True
finally:
    (out / 'summary.json').write_text(json.dumps({'passed': passed, 'checks': rows,
        'aad_sha256': hashlib.sha256(pathlib.Path(args.aad).read_bytes()).hexdigest(),
        'capture_helper_sha256': hashlib.sha256(pathlib.Path(args.capture_helper).read_bytes()).hexdigest()}, indent=2))
