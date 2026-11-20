import http.server, json, threading, urllib.request, sys
class H(http.server.BaseHTTPRequestHandler):
    def _h(self):
        n=int(self.headers.get('Content-Length') or 0); b=self.rfile.read(n) if n else b''
        print(self.command, self.path, dict(self.headers), b, flush=True)
        self.send_response(404); self.end_headers()
    do_GET=do_POST=do_PUT=_h
srv=http.server.HTTPServer(('0.0.0.0',10275),H)
threading.Thread(target=srv.serve_forever,daemon=True).start()
req=urllib.request.Request('http://192.0.2.20/local_reg.json',method='POST',
  data=json.dumps({"local_reg":{"ip":"192.0.2.10","notify":0,"port":10275,"uri":"/local_lan"}}).encode(),
  headers={'Content-Type':'application/json'})
print(urllib.request.urlopen(req,timeout=5).status, flush=True)
import time; time.sleep(15)
