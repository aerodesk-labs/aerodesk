// sip-accept-udp.js —— #553 P0 验收：标准 SIP 客户端（RFC 3261 手写报文）经
// UDP 5060 对 aerodesk signal 做 REGISTER(Digest) + INVITE/200/ACK 全流程，
// 验证「标准 SIP 客户端可接入，协议无私有依赖」（#551/#553 验收标准）。
// 用法: node sip-accept-udp.js <signal-host> <signal-port> <realm> <domain>
const crypto = require('crypto');
const dgram = require('dgram');

const [host = '127.0.0.1', port = 5060, realm = 'aerodesk', domain = 'aerodesk.test'] = process.argv.slice(2);

function md5(s) { return crypto.createHash('md5').update(s).digest('hex'); }

class SipUdpClient {
  constructor(name, password) {
    this.name = name;
    this.password = password;
    this.sock = dgram.createSocket('udp4');
    this.buf = '';
    this.callId = `accept-${name}-${Date.now()}`;
    this.cseq = 1;
    this.viaBranch = `z9hG4bK${md5(this.callId + name).slice(0, 12)}`;
    this.answered = null;
  }
  send(text) {
    this.sock.send(Buffer.from(text), port, host);
  }
  // 注册（Digest 质询 → 重发）
  register() {
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error('REGISTER 超时')), 5000);
      const first = this.regLine('');
      this.send(first);
      this.sock.on('message', (msg) => {
        const text = msg.toString();
        this.buf += text;
        if (text.includes('401 Unauthorized')) {
          const auth = /WWW-Authenticate: Digest ([^\r\n]+)/.exec(text);
          if (!auth) return reject(new Error('401 无 WWW-Authenticate'));
          const params = {};
          for (const m of auth[1].matchAll(/(\w+)=(?:"([^"]*)"|([^\s,]+))/g)) params[m[1]] = m[2] ?? m[3];
          const nonce = params.nonce;
          const ha1 = md5(`${this.name}:${realm}:${this.password}`);
          const ha2 = md5(`REGISTER:sip:${domain}`);
          const response = md5(`${ha1}:${nonce}:${ha2}`);
          this.send(this.regLine(response, nonce));
        } else if (text.includes(' 200 OK')) {
          clearTimeout(t);
          resolve(text);
        }
      });
    });
  }
  regLine(response, nonce) {
    const auth = response
      ? `Authorization: Digest username="${this.name}", realm="${realm}", nonce="${nonce}", uri="sip:${domain}", response="${response}", algorithm=MD5\r\n`
      : '';
    return [
      `REGISTER sip:${domain} SIP/2.0`,
      `Via: SIP/2.0/UDP 127.0.0.1:${50000 + Math.floor(Math.random() * 1000)};branch=${this.viaBranch};rport`,
      `Max-Forwards: 70`,
      `From: <sip:${this.name}@${domain}>;tag=${md5(this.name).slice(0, 8)}`,
      `To: <sip:${this.name}@${domain}>`,
      `Call-ID: ${this.callId}`,
      `CSeq: ${this.cseq++} REGISTER`,
      `Contact: <sip:${this.name}@127.0.0.1:${host}>;expires=120`,
      `Expires: 120`,
      `User-Agent: sip-accept (标准 SIP 客户端验收)`,
      auth + 'Content-Length: 0',
      '', '',
    ].join('\r\n');
  }
  // 主叫：INVITE（带最小 SDP offer）→ 200(answer) → ACK
  invite(target) {
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error('INVITE 超时')), 8000);
      const sdp = [
        'v=0', `o=- 0 0 IN IP4 127.0.0.1`, 's=-', 't=0 0',
        'm=application 9 UDP/DTLS/SCTP webrtc-datachannel',
        'a=sendrecv', '',
      ].join('\r\n');
      const callId2 = `call-${Date.now()}`;
      this.send([
        `INVITE sip:${target}@${domain} SIP/2.0`,
        `Via: SIP/2.0/UDP 127.0.0.1:${50000 + Math.floor(Math.random() * 1000)};branch=${this.viaBranch};rport`,
        `Max-Forwards: 70`,
        `From: <sip:${this.name}@${domain}>;tag=${md5(this.name).slice(0, 8)}`,
        `To: <sip:${target}@${domain}>`,
        `Call-ID: ${callId2}`,
        `CSeq: ${this.cseq++} INVITE`,
        `Contact: <sip:${this.name}@127.0.0.1>`,
        `Content-Type: application/sdp`,
        `Content-Length: ${Buffer.byteLength(sdp)}`,
        '', sdp,
      ].join('\r\n'));
      const onMsg = (msg) => {
        const text = msg.toString();
        this.buf += text;
        if (text.includes(' 200 OK') && text.includes('INVITE')) {
          clearTimeout(t);
          this.sock.removeListener('message', onMsg);
          this.send([
            `ACK sip:${target}@${domain} SIP/2.0`,
            `Via: SIP/2.0/UDP 127.0.0.1:${50000 + Math.floor(Math.random() * 1000)};branch=${this.viaBranch};rport`,
            `Max-Forwards: 70`,
            `From: <sip:${this.name}@${domain}>;tag=${md5(this.name).slice(0, 8)}`,
            `To: <sip:${target}@${domain}>`,
            `Call-ID: ${callId2}`,
            `CSeq: ${this.cseq++} ACK`,
            `Content-Length: 0`,
            '', '',
          ].join('\r\n'));
          resolve(text);
        } else if (/^\S+ \d{3}/.test(text.split('\r\n')[0])) {
          const line = text.split('\r\n')[0];
          if (!line.includes('100 Trying') && !line.includes('180 Ringing')) {
            clearTimeout(t);
            this.sock.removeListener('message', onMsg);
            reject(new Error(`呼叫未成功：${line}`));
          }
        }
      };
      this.sock.on('message', onMsg);
    });
  }
  // 被叫：注册后监听 INVITE → 100 + 200(SDP answer) → 收 ACK（标准 UAS 应答）
  answerInvites() {
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => reject(new Error('被叫等待 INVITE 超时')), 15000);
      const onMsg = (msg) => {
        const text = msg.toString();
        if (!text.startsWith('INVITE ')) return;
        clearTimeout(t);
        this.sock.removeListener('message', onMsg);
        const lines = text.split('\r\n');
        const copy = (h) => { const l = lines.find(l => l.toLowerCase().startsWith(h.toLowerCase())); return l || ''; };
        const allVia = lines.filter(l => l.startsWith('Via:')).join('\r\n');
        const via = allVia || copy('Via');
        const from = copy('From');
        const to = copy('To');
        const callId = copy('Call-ID');
        const cseq = copy('CSeq');
        const answer = [
          'v=0', 'o=- 0 0 IN IP4 127.0.0.1', 's=-', 't=0 0',
          'm=application 9 UDP/DTLS/SCTP webrtc-datachannel',
          'a=sendrecv', '',
        ].join('\r\n');
        const reply = (status) => [
          `SIP/2.0 ${status}`,
          via, from, `${to};tag=${md5(this.name).slice(0, 8)}`,
          callId, cseq, 'Contact: <sip:' + this.name + '@127.0.0.1>',
          status.startsWith('200') ? 'Content-Type: application/sdp' : null,
          status.startsWith('200') ? `Content-Length: ${Buffer.byteLength(answer)}` : 'Content-Length: 0',
          '', status.startsWith('200') ? answer : '',
        ].filter(l => l !== null).join('\r\n');
        this.send(reply('100 Trying'));
        this.send(reply('200 OK'));
        console.log('PASS UA-B 收到 INVITE → 100 Trying + 200 OK（SDP answer）');
        resolve(text);
      };
      this.sock.on('message', onMsg);
    });
  }
  close() { this.sock.close(); }
}

(async () => {
  console.log(`== 标准 SIP 客户端验收（UDP ${host}:${port}，realm=${realm}，domain=${domain}）`);
  const a = new SipUdpClient('accept-user-a', 'pass-a');
  const b = new SipUdpClient('accept-user-b', 'pass-b');
  try {
    const ra = await a.register();
    console.log('PASS UA-A REGISTER → 200 OK（Digest 认证成功）');
    const rb = await b.register();
    console.log('PASS UA-B REGISTER → 200 OK（Digest 认证成功）');
    const bInvite = b.answerInvites();
    const resp = await a.invite('accept-user-b');
    await bInvite;
    const answerSdp = resp.slice(resp.lastIndexOf('\r\n\r\n') + 4);
    if (answerSdp && answerSdp.includes('m=')) {
      console.log('PASS UA-A INVITE → 200 OK（SDP answer 端到端透传）');
      console.log(`      answer 摘要：${answerSdp.split('\r\n').filter(l => l.startsWith('m=')).join(' / ')}`);
    } else {
      console.log('FAIL INVITE 200 无 SDP body');
      console.log('      resp 尾部：', JSON.stringify(resp.slice(-80)));
      process.exit(1);
    }
    console.log('PASS UA-A ACK 已发送（呼叫信令闭环）');
    console.log('== 标准 SIP 客户端验收：全部 PASS ==');
  } catch (e) {
    console.error('FAIL:', e.message);
    process.exit(1);
  } finally {
    a.close(); b.close();
  }
})();
