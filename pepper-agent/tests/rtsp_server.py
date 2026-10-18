#!/usr/bin/env python3
"""A test RTSP server on 127.0.0.1, Python's standard library only: each
client gets ffmpeg's test pattern (H.264 over RTP, made by an ffmpeg of its
own) interleaved on its RTSP connection (RTP over TCP), behind Basic
authentication if a user is given. For pepper-agent's tests of streams:
ffmpeg's own RTSP client (mpv's) connects to it as to a camera.

Usage: tests/rtsp_server.py [--port N] [--user U --password P]
prints the address, then serves until killed. Or, from Python:
RtspServer(user=..., password=...).address, .drop() (ends every client's
stream, as a camera that restarts), .clients, .denied, .close().
"""

import argparse
import base64
import os
import socket
import subprocess
import tempfile
import threading
import time


class RtspServer:
    def __init__(self, port=0, user=None, password=None, size="640x360", rate=30):
        self.user, self.password, self.size, self.rate = user, password, size, rate
        self.listener = socket.create_server(("127.0.0.1", port))
        self.port = self.listener.getsockname()[1]
        self.clients = 0      # streams played
        self.denied = 0       # requests without the right credentials
        self.auth_seen = []   # the credentials clients sent, decoded
        self.conns = []
        self.lock = threading.Lock()
        self.closed = False
        threading.Thread(target=self._accept, daemon=True).start()

    @property
    def address(self):
        return f"rtsp://127.0.0.1:{self.port}/live"

    def _accept(self):
        while not self.closed:
            try:
                conn, _ = self.listener.accept()
            except OSError:
                return
            client = _Client(self, conn)
            with self.lock:
                self.conns.append(client)
            threading.Thread(target=client.serve, daemon=True).start()

    def drop(self):
        """Ends every client's stream, as a camera that restarts."""
        with self.lock:
            conns, self.conns = self.conns, []
        for c in conns:
            c.close()

    def close(self):
        self.closed = True
        self.listener.close()
        self.drop()

    def authorized(self, headers):
        if not self.user:
            return True
        given = headers.get("authorization", "")
        if given.lower().startswith("basic "):
            try:
                pair = base64.b64decode(given[6:]).decode()
            except ValueError:
                pair = ""
            self.auth_seen.append(pair)
            if pair == f"{self.user}:{self.password}":
                return True
        self.denied += 1
        return False


class _Client:
    def __init__(self, server, conn):
        self.server, self.conn = server, conn
        self.send_lock = threading.Lock()
        self.ffmpeg = None
        self.rtp = None
        self.sdp = None
        self.playing = False

    def close(self):
        if self.ffmpeg:
            self.ffmpeg.kill()
            self.ffmpeg.wait()
        for s in (self.rtp, self.conn):
            try:
                s and s.close()
            except OSError:
                pass

    def send(self, data):
        with self.send_lock:
            self.conn.sendall(data)

    def reply(self, cseq, status="200 OK", headers=(), body=b""):
        lines = [f"RTSP/1.0 {status}", f"CSeq: {cseq}", *headers]
        if body:
            lines.append(f"Content-Length: {len(body)}")
        self.send(("\r\n".join(lines) + "\r\n\r\n").encode() + body)

    def produce(self):
        """An ffmpeg sending the pattern as RTP to a port of ours; its SDP."""
        self.rtp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.rtp.bind(("127.0.0.1", 0))
        port = self.rtp.getsockname()[1]
        rate = self.server.rate
        # ffmpeg writes the SDP by name: in a new folder of our own, not at a
        # name in the shared temp folder anyone could take first.
        with tempfile.TemporaryDirectory(prefix="pepper-rtsp-") as tmp:
            sdp_file = os.path.join(tmp, "stream.sdp")
            self.ffmpeg = subprocess.Popen(
                ["ffmpeg", "-v", "error", "-re", "-f", "lavfi", "-i",
                 f"testsrc2=size={self.server.size}:rate={rate}", "-c:v", "libx264",
                 "-preset", "ultrafast", "-tune", "zerolatency", "-g", str(rate),
                 "-pix_fmt", "yuv420p", "-f", "rtp", "-sdp_file", sdp_file,
                 f"rtp://127.0.0.1:{port}?rtcpport={port}"],
                stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            for _ in range(100):
                if os.path.exists(sdp_file) and os.path.getsize(sdp_file) > 0:
                    break
                time.sleep(0.05)
            with open(sdp_file) as f:
                sdp = f.read()
        lines = []
        for line in sdp.strip().splitlines():
            if line.startswith("c="):
                line = "c=IN IP4 0.0.0.0"
            if line.startswith("m=video"):
                line = "m=video 0 RTP/AVP 96"
            lines.append(line)
        lines.append("a=control:track0")
        return ("\r\n".join(lines) + "\r\n").encode()

    def relay(self):
        while self.playing:
            try:
                packet = self.rtp.recv(65536)
                self.send(b"$\x00" + len(packet).to_bytes(2, "big") + packet)
            except OSError:
                return

    def serve(self):
        buf = b""
        try:
            while True:
                data = self.conn.recv(65536)
                if not data:
                    break
                buf += data
                while buf:
                    if buf[:1] == b"$":  # the client's RTCP, interleaved
                        if len(buf) < 4 or len(buf) < 4 + int.from_bytes(buf[2:4], "big"):
                            break
                        buf = buf[4 + int.from_bytes(buf[2:4], "big"):]
                        continue
                    if b"\r\n\r\n" not in buf:
                        break
                    head, rest = buf.split(b"\r\n\r\n", 1)
                    lines = head.decode("latin-1").split("\r\n")
                    headers = {}
                    for line in lines[1:]:
                        if ":" in line:
                            k, v = line.split(":", 1)
                            headers[k.strip().lower()] = v.strip()
                    length = int(headers.get("content-length", 0))
                    if len(rest) < length:
                        break
                    buf = rest[length:]
                    self.handle(lines[0].split(" ")[0], headers)
        except OSError:
            pass
        finally:
            self.playing = False
            self.close()

    def handle(self, method, h):
        cseq = h.get("cseq", "0")
        session = "Session: 7f3a9c01;timeout=60"
        if method == "OPTIONS":
            return self.reply(cseq, headers=["Public: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER"])
        if not self.server.authorized(h):
            return self.reply(cseq, "401 Unauthorized", ['WWW-Authenticate: Basic realm="pepper"'])
        if method == "DESCRIBE":
            if self.sdp is None:
                self.sdp = self.produce()
            return self.reply(cseq, headers=["Content-Type: application/sdp",
                                             f"Content-Base: {self.server.address}/"], body=self.sdp)
        if method == "SETUP":
            if "TCP" not in h.get("transport", ""):
                return self.reply(cseq, "461 Unsupported Transport")
            return self.reply(cseq, headers=["Transport: RTP/AVP/TCP;unicast;interleaved=0-1", session])
        if method == "PLAY":
            self.reply(cseq, headers=[session, "Range: npt=0.000-"])
            if not self.playing:
                self.playing = True
                self.server.clients += 1
                threading.Thread(target=self.relay, daemon=True).start()
            return
        if method == "GET_PARAMETER":
            return self.reply(cseq, headers=[session])
        if method == "TEARDOWN":
            self.reply(cseq, headers=[session])
            self.playing = False
            return self.close()
        self.reply(cseq, "405 Method Not Allowed")


if __name__ == "__main__":
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=8554)
    ap.add_argument("--user")
    ap.add_argument("--password")
    a = ap.parse_args()
    server = RtspServer(a.port, a.user, a.password)
    print(server.address, flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        server.close()
