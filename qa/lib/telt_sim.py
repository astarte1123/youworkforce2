"""Badge Teltonika simulato per i test e2e (connessione TCP Codec8/Codec12 verso il bridge).

Uso: python qa/lib/telt_sim.py --imei IMEI --port 5027 --ready F --got F [--sos] [--ctl F --res F] [--secs N]
  - handshake IMEI; scrive "accepted"/"rejected" in --ready
  - --sos: invia subito un record Codec8E con event IO 1 (mappa di default -> sos)
  - risponde a ogni comando codec12 con "OK:<comando>" e lo appende a --got
  - se compare il file --ctl invia un record AVL senza allarme e scrive in --res "ack" (il bridge
    l'ha accettato) o "closed" (il bridge ha chiuso: titolarità cambiata), poi termina
"""
import argparse, os, socket, struct, time


def crc16(d):
    c = 0
    for b in d:
        c ^= b
        for _ in range(8):
            c = (c >> 1) ^ 0xA001 if c & 1 else c >> 1
    return c


def avl(event_io):
    ts = int(time.time() * 1000)
    rec = struct.pack(">Q", ts) + b"\x01" + struct.pack(">ii", int(9.19e7), int(45.47e7)) + struct.pack(">HHBH", 0, 0, 7, 0)
    rec += struct.pack(">H", event_io) + struct.pack(">H", 0) + struct.pack(">H", 0) * 4 + struct.pack(">H", 0)
    data = b"\x8e\x01" + rec + b"\x01"
    return b"\x00" * 4 + struct.pack(">I", len(data)) + data + b"\x00" * 4


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--imei", required=True); ap.add_argument("--port", type=int, default=5027)
    ap.add_argument("--ready", required=True); ap.add_argument("--got", required=True)
    ap.add_argument("--sos", action="store_true"); ap.add_argument("--ctl"); ap.add_argument("--res")
    ap.add_argument("--secs", type=float, default=30)
    a = ap.parse_args()
    s = socket.create_connection(("127.0.0.1", a.port), timeout=10)
    imei = a.imei.encode()
    s.sendall(struct.pack(">H", len(imei)) + imei)
    if s.recv(1) != b"\x01":
        open(a.ready, "w").write("rejected"); return
    if a.sos:
        s.sendall(avl(1)); recv_exact(s, 4)
    open(a.ready, "w").write("accepted")
    s.settimeout(0.3)
    end = time.time() + a.secs
    while time.time() < end:
        if a.ctl and os.path.exists(a.ctl):
            s.settimeout(5)
            try:
                s.sendall(avl(0)); ack = recv_exact(s, 4)
            except OSError:
                ack = None
            open(a.res, "w").write("ack" if ack else "closed"); s.close(); return
        try:
            h = recv_exact(s, 8)
        except socket.timeout:
            continue
        if h is None:
            break
        dl = struct.unpack(">I", h[4:8])[0]
        s.settimeout(5); body = recv_exact(s, dl + 4); s.settimeout(0.3)
        d = body[:dl]
        if d[0] == 0x0C and d[2] == 0x05:
            size = struct.unpack(">I", d[3:7])[0]; cmd = d[7:7 + size].decode()
            open(a.got, "a").write(cmd + "\n")
            resp = ("OK:" + cmd).encode()
            inner = bytes([0x0C, 1, 6]) + struct.pack(">I", len(resp)) + resp + b"\x01"
            s.sendall(b"\x00" * 4 + struct.pack(">I", len(inner)) + inner + struct.pack(">I", crc16(inner)))
    s.close()


if __name__ == "__main__":
    main()
