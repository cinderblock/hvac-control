"""Ayla LAN-mode prototype: key exchange, then send commands and print what comes back.

python research/lan.py [seconds] [local_ip] [METHOD RESOURCE ...]
With no METHOD/RESOURCE pairs, GETs every property. Example module request:
  python research/lan.py 60 192.0.2.10 GET wifi_status.json
"""
import base64
import hmac
import http.server
import json
import os
import pathlib
import queue
import random
import string
import sys
import threading
import time
import urllib.request

from Crypto.Cipher import AES

PORT = 10275
dev = json.loads((pathlib.Path(os.environ['APPDATA']) / 'hvac-control' / 'devices.json').read_text())[0]
LAN_KEY = dev['lanip_key'].encode()
AC_IP = dev['lan_ip']
commands = queue.Queue()
state = {}


def hm(key, msg):
  return hmac.digest(key, msg, 'sha256')


class Enc:
  def __init__(self, msg):
    build = lambda m: hm(LAN_KEY, hm(LAN_KEY, m) + m)
    self.sign_key = build(msg + b'0')
    # CBC chain state carries across messages for the whole session.
    self.cipher = AES.new(build(msg + b'1'), AES.MODE_CBC, build(msg + b'2')[:16])


class Handler(http.server.BaseHTTPRequestHandler):
  def log_message(self, *a):
    pass

  def reply(self, obj=None, code=200):
    body = json.dumps(obj).encode() if obj is not None else b''
    self.send_response(code)
    self.send_header('Content-Type', 'application/json')
    self.send_header('Content-Length', str(len(body)))
    self.end_headers()
    self.wfile.write(body)

  def do_POST(self):
    body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
    if self.path.endswith('key_exchange.json'):
      k = body['key_exchange']
      r2 = ''.join(random.choices(string.ascii_letters + string.digits, k=16))
      t2 = time.monotonic_ns() % 2**40
      r1, t1 = k['random_1'].encode(), str(k['time_1']).encode()
      state['app'] = Enc(r1 + r2.encode() + t1 + str(t2).encode())
      state['dev'] = Enc(r2.encode() + r1 + str(t2).encode() + t1)
      state['seq'] = 0
      print('key exchange ok', k, flush=True)
      return self.reply({'random_2': r2, 'time_2': t2})
    d = state['dev']
    text = d.cipher.decrypt(base64.b64decode(body['enc'])).rstrip(b'\0')
    ok = base64.b64encode(hm(d.sign_key, text)).decode() == body['sign']
    print(self.path, 'sig ok' if ok else 'BAD SIG', text.decode(errors='replace'), flush=True)
    self.reply()

  def do_GET(self):
    if not self.path.endswith('commands.json'):
      return self.reply(code=404)
    try:
      data = commands.get_nowait()
      print('->', json.dumps(data), flush=True)
    except queue.Empty:
      data = {}
    text = json.dumps({'seq_no': state['seq'], 'data': data}).encode()
    state['seq'] += 1
    a = state['app']
    padded = text.ljust((len(text) + 15) // 16 * 16, b'\0')
    self.reply({'enc': base64.b64encode(a.cipher.encrypt(padded)).decode(),
                'sign': base64.b64encode(hm(a.sign_key, text)).decode()})


def register():
  body = {'local_reg': {'ip': sys.argv[2] if len(sys.argv) > 2 else '192.0.2.10',
                        'notify': int(not commands.empty()), 'port': PORT, 'uri': '/local_lan'}}
  req = urllib.request.Request(f'http://{AC_IP}/local_reg.json', data=json.dumps(body).encode(),
                               headers={'Content-Type': 'application/json'},
                               method='PUT' if 'app' in state else 'POST')
  try:
    urllib.request.urlopen(req, timeout=5)
  except Exception as e:
    print('local_reg failed', e, flush=True)


srv = http.server.ThreadingHTTPServer(('0.0.0.0', PORT), Handler)
threading.Thread(target=srv.serve_forever, daemon=True).start()
extra = sys.argv[3:]
for i, (method, resource) in enumerate(zip(extra[::2], extra[1::2])):
  path = resource.split('?')[0]
  commands.put({'cmds': [{'cmd': {'method': method, 'resource': resource,
                                  'uri': '/local_lan/' + path, 'data': '', 'cmd_id': i}}]})
if not extra:
  for i, p in enumerate(dev['properties']):
    commands.put({'cmds': [{'cmd': {'method': 'GET', 'resource': 'property.json?name=' + p['name'],
                                    'uri': '/local_lan/property/datapoint.json', 'data': '',
                                    'cmd_id': i}}]})
end = time.time() + float(sys.argv[1] if len(sys.argv) > 1 else 30)
while time.time() < end:
  register()
  time.sleep(1 if not commands.empty() else 10)
