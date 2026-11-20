#!/usr/bin/env python3
"""Real native privacy -> classifier -> router chain, using loopback fixtures only."""
import argparse
import contextlib
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import unittest


class Mock(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        self.server.records.append((self.path, body))
        refused = self.server.refuse.get(body.get('model'))
        if refused:
            data = b'{"error":{"message":"fixture refusal","type":"overloaded"}}'
            self.send_response(refused)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)
            return
        if self.path.endswith('/systemone'):
            if 'kinds' in body.get('state', {}):
                value = {'answers': {'levels': {'noul': 1}, 'work0': {'score': 3}, 'work1': {'score': 0}}}
            else:
                value = {'answers': {'level': {'choice': 'complex', 'confidence': 1},
                                     'intent': {'choice': 'none of these', 'confidence': 1},
                                     'effort': {'score': 2}}}
            data = json.dumps(value).encode()
            content_type = 'application/json'
        elif body.get('stream'):
            text = body['messages'][0]['content']
            events = [{'choices': [{'delta': {'content': text[n:n+3]}, 'index': 0}]}
                      for n in range(0, len(text), 3)]
            events.append({'choices': [], 'usage': {'prompt_tokens': 12, 'completion_tokens': 8}})
            data = ''.join('data: ' + json.dumps(v) + '\n\n' for v in events).encode() + b'data: [DONE]\n\n'
            content_type = 'text/event-stream'
        else:
            data = json.dumps({'model': body['model'], 'choices': [{'message': {
                'role': 'assistant', 'content': body['messages'][0]['content']}}],
                'usage': {'prompt_tokens': 12, 'completion_tokens': 8}}).encode()
            content_type = 'application/json'
        self.send_response(200)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class Routing(unittest.TestCase):
    args = None

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='zcalld-routing-')
        self.root = Path(self.temp.name).resolve()
        self.mock = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Mock)
        self.mock.daemon_threads = True
        self.mock.records = []
        self.mock.refuse = {}
        self.thread = threading.Thread(target=self.mock.serve_forever, daemon=True)
        self.thread.start()
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            self.port = probe.getsockname()[1]
        self.config = self.root / 'config.conf'
        self.groups = self.root / 'groups.json'
        self.usage = self.root / 'usage.jsonl'
        self.process = None
        self.log = tempfile.TemporaryFile()
        settings = self.root / 'settings.json'
        settings.write_text(json.dumps({'redact': True, 'redactPersonal': True, 'redactModelOff': True}), encoding="utf-8", newline="\n")
        key = self.root / 'redact.key'
        key.write_bytes(b'K' * 32)
        if os.name == 'posix':
            key.chmod(0o600)
        privacy = self.root / 'privacy.json'
        privacy.write_text(json.dumps({'worker': str(self.args.privacy_worker), 'settings': str(settings), 'key': str(key)}), encoding="utf-8", newline="\n")
        self.state = {'groups': [{'name': 'group/test', 'mode': 'order', 'members': [
            {'model': 'first', 'reasoning_levels': ['low', 'medium', 'high']},
            {'model': 'second', 'reasoning_levels': ['low', 'medium', 'high']}],
            'original': {'affinity': 'turn', 'classifier': 'decider/mock', 'effort': 'auto', 'rules': [
                {'use': 'first', 'intent': 'simple'}, {'use': 'second', 'intent': 'complex', 'useEffort': 'high'}]}}],
            'deciders': [{'id': 'decider', 'url': f'http://127.0.0.1:{self.mock.server_port}/v1', 'protocol': 'systemone'}],
            'usage_file': str(self.usage)}
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")
        self.config.write_text(
            f'[server]\nlisten=127.0.0.1:{self.port}\nunix_socket=none\nworkers=2\nqueue=4\n'
            f'[provider mock]\nbase_url=http://127.0.0.1:{self.mock.server_port}/v1\nprotocol=chat\n'
            '[route first]\nprovider=mock\nmodel=upstream-first\n'
            '[route second]\nprovider=mock\nmodel=upstream-second\n'
            '[route group/test]\nprovider=mock\nmodel=upstream-first\n'
            f'[plugin privacy]\npath={self.args.redact_plugin}\nconfig_file={privacy}\n'
            f'[plugin routing]\npath={self.args.routing_plugin}\nconfig={self.groups}\n', encoding="utf-8", newline="\n")

    def start(self):
        environment = os.environ.copy()
        environment.pop('NOTIFY_SOCKET', None)
        self.process = subprocess.Popen([str(self.args.binary), 'serve', '--config', str(self.config)],
                                        stdout=self.log, stderr=self.log, env=environment)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                self.log.seek(0)
                self.fail(self.log.read().decode())
            try:
                if self.call(None)[0] == 200:
                    return
            except OSError:
                pass
            time.sleep(.02)
        self.fail('gateway did not start')

    def tearDown(self):
        if self.process and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=4)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                self.fail('gateway did not stop')
        self.mock.shutdown()
        self.mock.server_close()
        self.thread.join(3)
        self.log.close()
        self.temp.cleanup()

    def call(self, body, session='fixture-session'):
        with contextlib.closing(http.client.HTTPConnection('127.0.0.1', self.port, timeout=8)) as connection:
            connection.request('GET' if body is None else 'POST', '/healthz' if body is None else '/v1/chat/completions',
                               None if body is None else json.dumps(body),
                               {'Content-Type': 'application/json', 'X-Session-Id': session})
            response = connection.getresponse()
            return response.status, response.read()

    def test_classifier_sees_masked_text_and_selects_real_upstream(self):
        self.start()
        original = 'Design this feature for jane.doe@acme.io'
        for stream in (False, True):
            status, data = self.call({'model': 'group/test', 'messages': [{'role': 'user', 'content': original}], 'stream': stream})
            self.assertEqual(status, 200, data)
            generated = [v for path, v in self.mock.records if path.endswith('/chat/completions')][-1]
            self.assertEqual(generated['model'], 'upstream-second')
            self.assertEqual(generated['reasoning_effort'], 'high')
            self.assertNotIn('jane.doe@acme.io', json.dumps(generated))
            if stream:
                text = ''.join(v['choices'][0]['delta'].get('content', '') for line in data.decode().splitlines()
                               if line.startswith('data: ') and line != 'data: [DONE]'
                               and (v := json.loads(line[6:])).get('choices'))
            else:
                text = json.loads(data)['choices'][0]['message']['content']
            self.assertEqual(text, original)
        decisions = [v for path, v in self.mock.records if path.endswith('/systemone')]
        self.assertTrue(any('kinds' in d['state'] for d in decisions))
        self.assertTrue(any('message' in d['state'] for d in decisions))
        self.assertNotIn('jane.doe@acme.io', json.dumps(decisions))
        # The same turn keeps its decision even when handled by a different worker.
        self.assertEqual(len([d for d in decisions if 'message' in d['state']]), 1)
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline and (not self.usage.exists() or len(self.usage.read_text(encoding="utf-8").splitlines()) < 2):
            time.sleep(.02)
        records = [json.loads(line) for line in self.usage.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(records[-1]['output_tokens'], 8)
        self.assertNotIn('jane.doe@acme.io', self.usage.read_text(encoding="utf-8"))

    def test_unavailable_group_never_generates(self):
        group = self.state['groups'][0]
        group['original'] = {}
        for member in group['members']:
            member['off'] = True
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")
        self.start()
        status, _ = self.call({'model': 'group/test', 'messages': [{'role': 'user', 'content': 'hello'}]})
        self.assertEqual(status, 503)
        self.assertEqual(self.mock.records, [])

    def test_round_robin_is_shared_across_workers(self):
        group = self.state['groups'][0]
        group['original'] = {'affinity': 'none'}
        group['mode'] = 'rotate'
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")
        self.start()
        for n in range(6):
            status, _ = self.call({'model': 'group/test', 'messages': [{'role': 'user', 'content': f'message {n}'}]})
            self.assertEqual(status, 200)
        models = [v['model'] for path, v in self.mock.records if path.endswith('/chat/completions')]
        self.assertEqual(models, ['upstream-first', 'upstream-second'] * 3)

    def test_explicit_refusal_falls_back_before_response_headers(self):
        self.state['groups'][0]['original'] = {'affinity': 'turn'}
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")
        self.mock.refuse['upstream-first'] = 429
        self.start()
        original = 'Contact jane.doe@acme.io'
        status, body = self.call({'model': 'group/test', 'messages': [{'role': 'user', 'content': original}]})
        self.assertEqual(status, 200, body)
        requests = [v for path, v in self.mock.records if path.endswith('/chat/completions')]
        self.assertEqual([v['model'] for v in requests], ['upstream-first', 'upstream-second'])
        self.assertNotIn('jane.doe@acme.io', json.dumps(requests))
        self.assertEqual(json.loads(body)['choices'][0]['message']['content'], original)

    def test_server_error_with_uncertain_generation_is_not_replayed(self):
        self.state['groups'][0]['original'] = {'affinity': 'turn'}
        self.groups.write_text(json.dumps(self.state), encoding="utf-8", newline="\n")
        self.mock.refuse['upstream-first'] = 500
        self.start()
        status, _ = self.call({'model': 'group/test', 'messages': [{'role': 'user', 'content': 'fixture'}]})
        self.assertEqual(status, 500)
        self.assertEqual(len(self.mock.records), 1)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('binary', 'routing-plugin', 'redact-plugin', 'privacy-worker'):
        parser.add_argument('--' + name, required=True, type=lambda v: Path(v).resolve())
    Routing.args, remaining = parser.parse_known_args()
    unittest.main(argv=[__file__, *remaining])
