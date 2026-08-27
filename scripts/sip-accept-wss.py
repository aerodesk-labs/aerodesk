#!/usr/bin/env python3
"""#553 P0 验收：标准 SIP 客户端（RFC 7118 WSS）完整呼叫流程。
REGISTER(Digest) ×2 → A INVITE B（无凭据 407 → 携被叫口令的 Proxy-Authorization
重试，跟进 #503-4 INVITE 授权门禁）→ B 100+200(SDP answer) → A ACK(同 CSeq)。
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

    async def invite(self, target, password):
        """主叫：INVITE → 407 时以「被叫口令」构造 Proxy-Authorization 重试
        （#503-4：呼叫方证明知道被叫口令；username/口令均为被叫侧）→ 200。
        返回 (最终响应, Call-ID, INVITE 的 CSeq)——2xx 的 ACK 必须同 CSeq。"""
        sdp = ("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\n"
               "m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=sendrecv\r\n")
        cid = f"call-{self.name}-{id(self)}"
        uri = f"sip:{target}@{DOMAIN}"

        def inv_hdr(cseq, auth=""):
            return (f"INVITE {uri} SIP/2.0\r\n"
                    f"Via: SIP/2.0/WSS {HOST}:{PORT};branch={self.branch}\r\n"
                    f"Max-Forwards: 70\r\n"
                    f"From: <sip:{self.name}@{DOMAIN}>;tag={md5(self.name)[:8]}\r\n"
                    f"To: <sip:{target}@{DOMAIN}>\r\n"
                    f"Call-ID: {cid}\r\n"
                    f"CSeq: {cseq} INVITE\r\n"
                    f"Contact: <sip:{self.name}@{HOST}:{PORT}>\r\n"
                    + auth +
                    f"Content-Type: application/sdp\r\n"
                    f"Content-Length: {len(sdp.encode())}\r\n\r\n{sdp}")

        self.cseq += 1
        await self.send(inv_hdr(self.cseq))
        r = await self.recv_until(lambda m: "SIP/2.0 200" in m or "SIP/2.0 407" in m)
        if " 407" in r:
            nonce = re.search(r'nonce="([^"]+)"', r).group(1)
            ha1 = md5(f"{target}:{REALM}:{password}")
            ha2 = md5(f"INVITE:{uri}")
            resp = md5(f"{ha1}:{nonce}:{ha2}")
            pa = ('Proxy-Authorization: Digest '
                  f'username="{target}", realm="{REALM}", nonce="{nonce}", '
                  f'uri="{uri}", response="{resp}", algorithm=MD5\r\n')
            self.cseq += 1
            await self.send(inv_hdr(self.cseq, pa))
            r = await self.recv_until(lambda m: "SIP/2.0 200" in m or "SIP/2.0 4" in m)
        if " 200 OK" not in r:
            raise RuntimeError(f"INVITE 失败: {(r.splitlines() or ['<空>'])[0]}")
        return r, cid, self.cseq

    async def ack(self, target, cid, cseq):
        """2xx 的 ACK：与被确认的 INVITE 同 Call-ID + 同 CSeq（RFC 3261 §13.2.2）。"""
        await self.send(
            f"ACK sip:{target}@{DOMAIN} SIP/2.0\r\n"
            f"Via: SIP/2.0/WSS {HOST}:{PORT};branch={self.branch}\r\n"
            f"Max-Forwards: 70\r\n"
            f"From: <sip:{self.name}@{DOMAIN}>;tag={md5(self.name)[:8]}\r\n"
            f"To: <sip:{target}@{DOMAIN}>\r\n"
            f"Call-ID: {cid}\r\n"
            f"CSeq: {cseq} ACK\r\nContent-Length: 0\r\n\r\n")

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
        resp, cid, icseq = await a.invite("accept-wss-b", b.password)
        await b_task
        if "m=application" in resp:
            print("PASS UA-A INVITE(407 应答后) → 200 OK（SDP answer 端到端透传）")
        else:
            print("FAIL INVITE 200 无 SDP body")
            sys.exit(1)
        await a.ack("accept-wss-b", cid, icseq)
        print("PASS UA-A ACK 已发送（WSS 呼叫信令闭环）")
        print("== 标准 SIP 客户端完整呼叫流程（WSS）：全部 PASS ==")
    finally:
        for u in (a, b):
            if hasattr(u, "_conn"):
                await u._conn.__aexit__(None, None, None)


if __name__ == "__main__":
    asyncio.run(main())
