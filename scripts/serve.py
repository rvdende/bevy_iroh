#!/usr/bin/env python3
"""Serve the web/ bundle: `serve.py [port]` over http, `serve.py --https [port]` over TLS with a
self-signed certificate made on first use (web/tls/, ignored by git), `--dir site` for the
assembled site. Microphones, cameras and iroh's relay probes need a secure context, so anything
but localhost wants --https; the browser will warn about the certificate once."""
import http.server, os, ssl, subprocess, sys

here = os.path.dirname(os.path.abspath(__file__))
root = os.path.join(os.path.dirname(here), "web")
https = "--https" in sys.argv
args = [a for a in sys.argv[1:] if a != "--https"]
if "--dir" in args:
    at = args.index("--dir")
    root = os.path.abspath(args[at + 1])
    del args[at:at + 2]
port = int(args[0]) if args else (8443 if https else 8000)


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *a, **k):
        super().__init__(*a, directory=root, **k)

    def end_headers(self):
        # wasm is fetched a lot while iterating; never let a stale bundle be cached.
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


server = http.server.ThreadingHTTPServer(("0.0.0.0", port), Handler)
if https:
    tls = os.path.join(os.path.dirname(here), "web", "tls")
    cert, key = os.path.join(tls, "cert.pem"), os.path.join(tls, "key.pem")
    if not os.path.exists(cert):
        os.makedirs(tls, exist_ok=True)
        subprocess.run(
            ["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "30",
             "-keyout", key, "-out", cert, "-subj", "/CN=bevy_iroh",
             "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"],
            check=True, capture_output=True,
        )
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
print(f"serving {root} on {'https' if https else 'http'}://0.0.0.0:{port}/?join=<ticket>", flush=True)
server.serve_forever()
