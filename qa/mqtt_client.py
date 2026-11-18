#!/usr/bin/env python3
# Mini-client MQTT 3.1.1 senza dipendenze (socket raw) per le suite e2e del broker nativo YW.
# Uso:
#   mqtt_client.py connrc <host> <port> <user>                 -> stampa "CONNACK <rc>"
#   mqtt_client.py pub    <host> <port> <user> <topic> <json>  -> connette, pubblica QoS0, stampa CONNACK
#   mqtt_client.py sub    <host> <port> <user> <topic> <secs>  -> sottoscrive, stampa "MSG <payload>" per T secondi
import socket, sys

def enc_len(n):
    out = b''
    while True:
        b = n % 128; n //= 128
        if n > 0: b |= 0x80
        out += bytes([b])
        if n == 0: break
    return out

def mstr(s):
    b = s.encode(); return len(b).to_bytes(2, 'big') + b

def mk_connect(client_id, username):
    var = mstr("MQTT") + bytes([4]) + bytes([0x82]) + (60).to_bytes(2, 'big')  # flags: username|clean
    body = var + mstr(client_id) + mstr(username)
    return b'\x10' + enc_len(len(body)) + body

def mk_publish(topic, payload):
    body = mstr(topic) + payload
    return b'\x30' + enc_len(len(body)) + body

def mk_subscribe(pid, topic):
    body = pid.to_bytes(2, 'big') + mstr(topic) + bytes([0])
    return b'\x82' + enc_len(len(body)) + body

def recv_n(s, n):
    buf = b''
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk: raise EOFError()
        buf += chunk
    return buf

def read_len(s):
    mult = 1; val = 0
    while True:
        b = recv_n(s, 1)[0]
        val += (b & 0x7f) * mult
        if b & 0x80 == 0: break
        mult *= 128
    return val

def read_packet(s):
    b0 = recv_n(s, 1)[0]
    rem = read_len(s)
    body = recv_n(s, rem) if rem else b''
    return b0 >> 4, b0 & 0x0f, body

def connect(host, port, user):
    s = socket.create_connection((host, int(port)), timeout=5)
    s.sendall(mk_connect("e2e-" + user[:8], user))
    t, _f, body = read_packet(s)  # CONNACK
    rc = body[1] if (t == 2 and len(body) >= 2) else 255
    return s, rc

def main():
    mode = sys.argv[1]
    host, port, user = sys.argv[2], sys.argv[3], sys.argv[4]
    if mode == "connrc":
        try:
            s, rc = connect(host, port, user); s.close()
        except Exception:
            rc = 255
        print("CONNACK", rc); return
    if mode == "pub":
        topic, payload = sys.argv[5], sys.argv[6].encode()
        s, rc = connect(host, port, user)
        print("CONNACK", rc)
        if rc == 0:
            s.sendall(mk_publish(topic, payload))
            s.sendall(b'\xe0\x00')  # DISCONNECT
        s.close(); return
    if mode == "sub":
        topic, secs = sys.argv[5], float(sys.argv[6])
        s, rc = connect(host, port, user)
        print("CONNACK", rc)
        if rc != 0: s.close(); return
        s.sendall(mk_subscribe(1, topic))
        s.settimeout(secs)
        try:
            while True:
                t, _f, body = read_packet(s)
                if t == 9:  # SUBACK
                    continue
                if t == 3:  # PUBLISH dal broker
                    tl = int.from_bytes(body[0:2], 'big')
                    payload = body[2 + tl:]
                    print("MSG", payload.decode('utf-8', 'replace'))
        except Exception:
            pass
        s.close(); return

if __name__ == "__main__":
    main()
