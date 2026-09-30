"""Static file server for the gui/web bundle, header-correct for WebAssembly.

Usage: python3 serve_web.py [port] [--coop]
  port   listen port, default 8090
  --coop also send the cross-origin isolation headers (COOP/COEP)
"""

import sys
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer


class Handler(SimpleHTTPRequestHandler):
    extensions_map = {
        **SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
        ".svg": "image/svg+xml",
    }

    def end_headers(self):
        if self.server.coop:
            self.send_header("Cross-Origin-Opener-Policy", "same-origin")
            self.send_header("Cross-Origin-Embedder-Policy", "require-corp")
        self.send_header("Cache-Control", "no-store")
        super().end_headers()


def main():
    port = 8090
    coop = False
    args = [a for a in sys.argv[1:] if not a.startswith("-")]
    coop = "--coop" in sys.argv[1:]
    if args:
        port = int(args[0])
    server = ThreadingHTTPServer(
        ("127.0.0.1", port), partial(Handler, directory="web")
    )
    server.coop = coop
    print(f"serving gui/web on http://127.0.0.1:{port} coop={coop}")
    server.serve_forever()


if __name__ == "__main__":
    main()
