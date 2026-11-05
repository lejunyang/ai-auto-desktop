"""Loopback-only CDP proxy withholding actual getResponseBody replies."""
import http.server
import json
import socket
import struct
import threading
import urllib.request

def server_frame(message):
    body = json.dumps(message).encode()
    size = len(body)
    return bytes([129, size]) + body if size < 126 else bytes([129, 126]) + struct.pack('!H', size) + body

def frame(stream):
    def exact(n):
        data = bytearray()
        while len(data) < n:
            chunk = stream.read(n-len(data))
            if not chunk:
                raise EOFError()
            data.extend(chunk)
        return bytes(data)
    head = exact(2)
    size = head[1] & 127
    extra = exact(2 if size == 126 else 8 if size == 127 else 0)
    if extra:
        size = int.from_bytes(extra, 'big')
    mask = exact(4 if head[1] & 128 else 0)
    body = exact(size)
    decoded = bytes(c ^ mask[i % 4] for i,c in enumerate(body)) if mask else body
    return head+extra+mask+body, decoded, head[0] & 15

class Proxy:
    def __init__(self, port, hold=None, drop_event=None, on_held=None):
        self.commands = []
        self.condition = threading.Condition()
        self.received = threading.Event()
        self.held = threading.Event()
        self.peers = []
        proxy = self
        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'
            def log_message(self, *args): pass
            def do_GET(self):
                if self.headers.get('Upgrade', '').lower() != 'websocket':
                    with urllib.request.urlopen(f'http://127.0.0.1:{port}{self.path}') as response:
                        data = response.read().replace(f'127.0.0.1:{port}'.encode(), f'127.0.0.1:{proxy.server.server_port}'.encode())
                    self.send_response(200)
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                    return
                upstream = socket.create_connection(('127.0.0.1', port), timeout=5)
                upstream.settimeout(None)
                proxy.peers.extend([upstream, self.connection])
                headers = ''.join(f'{k}: {v}\r\n' for k,v in self.headers.items() if k.lower() != 'host')
                upstream.sendall(f'GET {self.path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}\r\n'.encode())
                source = upstream.makefile('rb')
                assert b'101' in source.readline()
                self.send_response(101)
                while True:
                    line = source.readline()
                    if line == b'\r\n': break
                    key,value = line.decode().split(':',1)
                    self.send_header(key,value.strip())
                self.end_headers()
                self.wfile.flush()
                bodies = {}
                def to_browser():
                    try:
                        while True:
                            raw,data,opcode = frame(self.rfile)
                            if opcode == 1:
                                message = json.loads(data)
                                with proxy.condition:
                                    proxy.commands.append((self.path, message))
                                    proxy.condition.notify_all()
                                if (hold(self.path, message) if hold else message.get('method') == 'Network.getResponseBody'):
                                    bodies[message['id']] = message
                                    proxy.received.set()
                            upstream.sendall(raw)
                    except (EOFError, OSError, ValueError): pass
                threading.Thread(target=to_browser,daemon=True).start()
                try:
                    while True:
                        raw,data,opcode = frame(source)
                        if opcode == 1 and drop_event and drop_event(self.path, json.loads(data)):
                            continue
                        if opcode == 1 and json.loads(data).get('id') in bodies:
                            # Reply really arrived from Chromium. Never deliver it.
                            proxy.held.set()
                            if on_held:
                                for message in on_held(self.path, bodies[json.loads(data)['id']]):
                                    self.connection.sendall(server_frame(message))
                            continue
                        self.connection.sendall(raw)
                except (EOFError, OSError): pass
        self.server = http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever,daemon=True).start()

    def close(self):
        for peer in self.peers:
            try: peer.shutdown(socket.SHUT_RDWR)
            except OSError: pass
            peer.close()
        self.server.shutdown()
