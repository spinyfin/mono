"""A loopback Bazel HTTP remote cache for lost_inputs_integration.sh.

Usage: loopback_cache.py <data dir> <port file>

Stores GET/PUT/HEAD /ac/<sha> and /cas/<sha> as files named ac_<sha> and
cas_<sha> in the data dir, so the script can make a blob unavailable remotely by
deleting its file. Binds an ephemeral port and writes it to the port file.
"""
import http.server
import os
import sys

root, port_file = sys.argv[1], sys.argv[2]


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def _file(self):
        return os.path.join(root, self.path.strip("/").replace("/", "_"))

    def _reply(self, code, body=b"", head=False):
        self.send_response(code)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if not head:
            self.wfile.write(body)

    def do_GET(self):
        path = self._file()
        if os.path.exists(path):
            with open(path, "rb") as f:
                self._reply(200, f.read())
        else:
            self._reply(404)

    def do_HEAD(self):
        self._reply(200 if os.path.exists(self._file()) else 404, head=True)

    def do_PUT(self):
        length = int(self.headers.get("Content-Length", 0))
        with open(self._file(), "wb") as f:
            f.write(self.rfile.read(length))
        self._reply(200)


server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
with open(port_file, "w") as f:
    f.write(str(server.server_address[1]))
server.serve_forever()
