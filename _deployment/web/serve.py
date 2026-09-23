import http.server, sys, os
class H(http.server.SimpleHTTPRequestHandler):
    def end_headers(self):
        self.send_header("Cross-Origin-Opener-Policy", "same-origin"); self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        super().end_headers()
os.chdir(sys.argv[1]); http.server.ThreadingHTTPServer(("127.0.0.1", int(sys.argv[2])), H).serve_forever()
