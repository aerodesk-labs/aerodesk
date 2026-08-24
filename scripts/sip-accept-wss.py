#!/usr/bin/env python3
"""#553 P0 验收：标准 SIP 客户端（RFC 7118 WSS）完整呼叫流程。
REGISTER(Digest) ×2 → A INVITE B → B 100+200(SDP answer) → A ACK。
"""
import asyncio, hashlib, ssl, re, sys

HOST, PORT, REALM, DOMAIN = "127.0.0.1", 3061, "aerodesk", "aerodesk.test"
URL = f"wss://{HOST}:{PORT}"


def md5(s):
    return hashlib.md5(s.encode()).hexdigest()


class UA:
    def __init__(self, name, password):
        self.name, self.password = name, password
        self.cseq = 0
        self.call_id = f"accept-{name}-{id(self)}"
        self.branch = f"z9hG4bK{md5(self.call_id)[:12]}"

    async def connect(self):
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        self._conn = __import__("websockets").connect(URL, ssl=ctx, subprotocols=["sip"])
        self.ws = await self._conn.__aenter__()

    async def send(self, text):
        await self.ws.send(text)

    async def recv_until(self, pred, timeout=8.0):
        async def loop():
            while True:
                m = await self.ws.recv()
                if pred(m):
                    return m
        return await asyncio.wait_for(loop(), timeout=timeout)

    def headers(self, method, auth="", extra_call_id=None, contact_port=None):
        self.cseq += 1
        return (
            f"Via: SIP/2.0/WSS {HOST}:{PORT};branch={self.branch}\r\n"
            f"Max-Forwards: 70\r\n"
            f"From: <sip:{self.name}@{DOMAIN}>;tag={md5(self.name)[:8]}\r\n"
            f"To: <sip:{self.name}@{DOMAIN}>\r\n"
            f"Call-ID: {extra_call_id or self.call_id}\r\n"
            f"CSeq: {self.cseq} {method}\r\n"
            f"Contact: <sip:{self.name}@{HOST}:{contact_port or PORT}>\r\n"
            + auth
        )

    async def register(self):
        await self.send(f"REGISTER sip:{DOMAIN} SIP/2.0\r\n" + self.headers("REGISTER") + "Expires: 120\r\nContent-Length: 0\r\n\r\n")
        r1 = await self.recv_until(lambda m: "401 Unauthorized" in m)
        nonce = re.search(r'nonce="([^"]+)"', r1).group(1)
        ha1 = md5(f"{self.name}:{REALM}:{self.password}")
        ha2 = md5(f"REGISTER:sip:{DOMAIN}")
        resp = md5(f"{ha1}:{nonce}:{ha2}")
        auth = (f'Authorization: Digest username="{self.name}", realm="{REALM}", '
                f'nonce="{nonce}", uri="sip:{DOMAIN}", response="{resp}", algorithm=MD5\r\n')
        await self.send(f"REGISTER sip:{DOMAIN} SIP/2.0\r\n" + self.headers("REGISTER", auth) + "Expires: 120\r\nContent-Length: 0\r\n\r\n")
        r2 = await self.recv_until(lambda m: " 200 OK" in m or " 401" in m)
        if " 200 OK" not in r2:
            raise RuntimeError(f"REGISTER 失败: {r2.split(chr(13))[0]}")
        print(f"PASS UA-{self.name} REGISTER → 200 OK（WSS + Digest 认证成功）")

    async def invite(self, target):
        sdp = ("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n"
               "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sendrecv\r\n")
        cid = f"call-{self.name}-{id(self)}"
        hdr = (f"INVITE sip:{target}@{DOMAIN} SIP/2.0\r\n"
               f"Via: SIP/2.0/WSS {HOST}:{PORT};branch={self.branch}\r\n"
               f"Max-Forwards: 70\r\n"
               f"From: <sip:{self.name}@{DOMAIN}>;tag={md5(self.name)[:8]}\r\n"
               f"To: <sip:{target}@{DOMAIN}>\r\n"
               f"Call-ID: {cid}\r\n"
               f"CSeq: 1 INVITE\r\n"
               f"Contact: <sip:{self.name}@{HOST}:{PORT}>\r\n"
               f"Content-Type: application/sdp\r\n"
               f"Content-Length: {len(sdp.encode())}\r\n\r\n{sdp}")
        await self.send(hdr)
        return await self.recv_until(lambda m: " 200 OK" in m and "INVITE" in m), cid

    async def ack(self, target, cid):
        await self.send(
            f"ACK sip:{target}@{DOMAIN} SIP/2.0\r\n"
            f"Via: SIP/2.0/WSS {HOST}:{PORT};branch={self.branch}\r\n"
            f"Max-Forwards: 70\r\n"
            f"From: <sip:{self.name}@{DOMAIN}>;tag={md5(self.name)[:8]}\r\n"
            f"To: <sip:{target}@{DOMAIN}>\r\n"
            f"Call-ID: {cid}\r\n"
            f"CSeq: 2 ACK\r\nContent-Length: 0\r\n\r\n")

    async def answer(self):
        """被叫 UAS：收 INVITE → 100 + 200(SDP answer)"""
        inv = await self.recv_until(lambda m: m.startswith("INVITE "))
        lines = inv.split("\r\n")
        copy = lambda h: next((l for l in lines if l.lower().startswith(h.lower())), "")
        all_via = "\r\n".join(l for l in lines if l.startswith("Via:"))
        answer = "v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sendrecv\r\n"
        await self.send(f"SIP/2.0 100 Trying\r\n{all_via}\r\n{copy('From')}\r\n{copy('To')};tag={md5(self.name)[:8]}\r\n{copy('Call-ID')}\r\n{copy('CSeq')}\r\nContent-Length: 0\r\n\r\n")
        await self.send(f"SIP/2.0 200 OK\r\n{all_via}\r\n{copy('From')}\r\n{copy('To')};tag={md5(self.name)[:8]}\r\n{copy('Call-ID')}\r\n{copy('CSeq')}\r\nContact: <sip:{self.name}@{HOST}:{PORT}>\r\nContent-Type: application/sdp\r\nContent-Length: {len(answer.encode())}\r\n\r\n{answer}")
        return inv


async def main():
    print("== 标准 SIP 客户端完整呼叫流程（WSS）")
    a, b = UA("accept-wss-a", "pass-a"), UA("accept-wss-b", "pass-b")
    try:
        await a.connect()
        await b.connect()
        await a.register()
        await b.register()
        b_task = asyncio.create_task(b.answer())
        resp, cid = await a.invite("accept-wss-b")
        await b_task
        if "m=application" in resp:
            print("PASS UA-A INVITE → 200 OK（SDP answer 端到端透传）")
        else:
            print("FAIL INVITE 200 无 SDP body")
            sys.exit(1)
        await a.ack("accept-wss-b", cid)
        print("PASS UA-A ACK 已发送（WSS 呼叫信令闭环）")
        print("== 标准 SIP 客户端完整呼叫流程（WSS）：全部 PASS ==")
    finally:
        for u in (a, b):
            if hasattr(u, "_conn"):
                await u._conn.__aexit__(None, None, None)


if __name__ == "__main__":
    asyncio.run(main())
