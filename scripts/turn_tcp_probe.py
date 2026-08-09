#!/usr/bin/env python3
"""SFU 内嵌 TURN TCP/TLS 互操作探针（#196）：
独立实现（非 Rust）：TCP/TLS + RFC4571 帧 → Allocate(401) → CreatePermission → ChannelBind
→ ChannelData → peer(relay) 收到明文；peer 回 relayed → 客户端收到 ChannelData。
用法: turn_tcp_probe.py <host> <port> <secret> [--tls] [--tls-cert cert.pem]
"""
import socket, ssl, struct, os, hmac, hashlib, base64, time, sys

MAGIC = 0x2112A442
ATTR = {
    'USERNAME': 0x0006, 'REALM': 0x0014, 'NONCE': 0x0015,
    'MI': 0x0008, 'ERROR': 0x0009, 'REQ_TRANS': 0x0019,
    'XOR_PEER': 0x0012, 'XOR_RELAYED': 0x0016, 'CHANNEL': 0x000c,
    'DATA': 0x0013, 'LIFETIME': 0x000d,
}

def attr(t, v):
    pad = (4 - (len(v) % 4)) % 4
    return struct.pack("!HH", t, len(v)) + v + b"\x00" * pad

def header(mt, body, tid):
    return struct.pack("!HHI", mt, len(body), MAGIC) + tid + body

def stun(mt, txid, attrs, auth=None):
    body = b"".join(attr(t, v) for t, v in attrs)
    if auth:
        user, password, realm, nonce = auth
        body += attr(ATTR['USERNAME'], user.encode()) + attr(ATTR['REALM'], realm.encode()) + attr(ATTR['NONCE'], nonce.encode())
        key = hashlib.md5(f"{user}:{realm}:{password}".encode()).digest()
        body += struct.pack("!HH", ATTR['MI'], 20) + b"\x00" * 20
        msg = header(mt, body, txid)
        # RFC 5389：HMAC 输入不含 MI 属性头+值（msg[:-24]）
        mac = hmac.new(key, msg[:-24], hashlib.sha1).digest()
        body = body[:-20] + mac  # 保留 MI 属性头（4 字节），替换 20 字节值
    return header(mt, body, txid)

def parse(pkt):
    out = {}
    i = 20
    while i + 4 <= len(pkt):
        t, l = struct.unpack("!HH", pkt[i:i+4])
        out[t] = pkt[i+4:i+4+l]
        i += 4 + ((l + 3) & ~3)
    return out

def xor_addr(v):
    port = struct.unpack("!H", v[2:4])[0] ^ (MAGIC >> 16)
    magic = struct.pack("!I", MAGIC)
    a = bytes(b ^ magic[i % 4] for i, b in enumerate(v[4:8]))
    return f"{'.'.join(map(str, a))}:{port}"

def send_frame(s, data):
    s.sendall(struct.pack("!H", len(data)) + data)

def read_frame(s):
    lb = b""
    while len(lb) < 2:
        c = s.recv(2 - len(lb))
        if not c: raise EOFError("eof")
        lb += c
    ln = struct.unpack("!H", lb)[0]
    body = b""
    while len(body) < ln:
        c = s.recv(ln - len(body))
        if not c: raise EOFError("eof")
        body += c
    return body

def allocate(s, user, password):
    tid = os.urandom(12)
    send_frame(s, stun(0x0003, tid, [(ATTR['REQ_TRANS'], b"\x11\x00\x00\x00")]))
    resp = parse(read_frame(s))
    realm = resp[ATTR['REALM']].decode(); nonce = resp[ATTR['NONCE']].decode()
    err = (resp[ATTR['ERROR']][2] & 7) * 100 + resp[ATTR['ERROR']][3]
    assert err == 401, f"expected 401, got {err}"
    tid = os.urandom(12)
    send_frame(s, stun(0x0003, tid, [(ATTR['REQ_TRANS'], b"\x11\x00\x00\x00")],
                       (user, password, realm, nonce)))
    resp = parse(read_frame(s))
    relayed = xor_addr(resp[ATTR['XOR_RELAYED']])
    return relayed, realm, nonce

def main():
    host, port, secret = sys.argv[1], int(sys.argv[2]), sys.argv[3]
    tls = "--tls" in sys.argv
    cert = None
    if "--tls-cert" in sys.argv:
        cert = sys.argv[sys.argv.index("--tls-cert") + 1]
    raw = socket.create_connection((host, port), timeout=5)
    if tls:
        ctx = ssl.create_default_context(cafile=cert)
        raw = ctx.wrap_socket(raw, server_hostname="str0m.test")
    s = raw
    s.settimeout(5)
    now = int(time.time()) + 3600
    username = f"{now}:probe"
    password = base64.b64encode(hmac.new(secret.encode(), username.encode(), hashlib.sha1).digest()).decode()
    relayed, realm, nonce = allocate(s, username, password)
    print("PASS allocate over", "TLS" if tls else "TCP", "relayed=", relayed)

    # peer（纯 UDP，模拟对端）
    peer = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    peer.bind(("127.0.0.1", 0))
    peer.settimeout(5)
    peer_addr = f"127.0.0.1:{peer.getsockname()[1]}"
    host_ip, relay_port = relayed.split(":")
    peer_host = socket.gethostbyname(host_ip) if host_ip not in ("0.0.0.0",) else "127.0.0.1"
    relayed_sockaddr = (peer_host, int(relay_port))

    # CreatePermission + ChannelBind
    tid = os.urandom(12)
    send_frame(s, stun(0x0008, tid, [(ATTR['XOR_PEER'], xor_peer_bytes(peer_addr))], (username, password, realm, nonce)))
    assert parse(read_frame(s)).get(ATTR['ERROR']) is None, "create permission failed"
    chan = 0x4000
    tid = os.urandom(12)
    send_frame(s, stun(0x0009, tid, [(ATTR['CHANNEL'], struct.pack("!H", chan)),
                                     (ATTR['XOR_PEER'], xor_peer_bytes(peer_addr))],
                       (username, password, realm, nonce)))
    assert parse(read_frame(s)).get(ATTR['ERROR']) is None, "channel bind failed"

    # ChannelData → relay → peer（明文 UDP）
    payload = b"hello-tcp-turn-probe"
    send_frame(s, struct.pack("!HH", chan, len(payload)) + payload)
    data, src = peer.recvfrom(128)
    assert data == payload, f"peer payload mismatch: {data!r}"
    print("PASS ChannelData relayed to peer")

    # peer → relayed → server → ChannelData → 客户端
    peer.sendto(b"reply-tcp-turn-probe", relayed_sockaddr)
    raw = read_frame(s)
    assert raw[:2] == struct.pack("!H", chan), "expected channeldata reply"
    ln = struct.unpack("!H", raw[2:4])[0]
    assert raw[4:4+ln] == b"reply-tcp-turn-probe", "reply payload mismatch"
    print("PASS peer reply relayed back")
    print("RESULT: OK")

def xor_peer_bytes(addr):
    ip, port = addr.rsplit(":", 1)
    ipb = socket.inet_aton(ip)
    port = int(port) ^ (MAGIC >> 16)
    magic = struct.pack("!I", MAGIC)
    return bytes([0, 1]) + struct.pack("!H", port) + bytes(b ^ magic[i] for i, b in enumerate(ipb))

if __name__ == "__main__":
    main()
