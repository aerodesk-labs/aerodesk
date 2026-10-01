// Web 观看页（sip-viewer.html，UAC）→ 原生 CLI publisher（SIP UAS）方向 e2e 的
// 浏览器侧驱动。与 scripts/web-edge-e2e-run.js 的区别：被叫是**原生** CLI publisher，
// 不在浏览器里；本脚本只驱动观看页，输入回传的落地断言在 publisher 日志（ps1 侧）。
//
// 由 scripts/web-view-native-e2e.ps1 调用：
//   NODE_PATH=<e2eDir>\node_modules node web-view-native-e2e-run.js
// 环境变量：WEB_SERVE_PORT / ROOM / SIGNAL_URL / TOKEN / VIEWER_DEVICE
//
// 输出约定（ps1 侧 grep）：VIDEO_READY= / INPUT_CHANNEL_OPEN= / INPUT_SENT= /
// PEER_CANDIDATE= / PEER_PORT= / RECEIVERS= / OFFER_MLINES= / ANSWER_MLINES=，
// 失败时打印 ---PAGELOG--- / ---STATUS--- 现场。退出码：0=浏览器侧全部通过。
const { chromium } = require('playwright-core');

const PORT = process.env.WEB_SERVE_PORT || 38084;
const ROOM = process.env.ROOM || process.argv[2];
const SIGNAL = process.env.SIGNAL_URL || 'wss://127.0.0.1:3061';
const TOKEN = process.env.TOKEN || '';
const VIEWER = process.env.VIEWER_DEVICE || ('web-viewer-' + Date.now().toString(16).slice(-8));

function fail(msg) { console.error('E2E FAIL:', msg); process.exit(1); }

(async () => {
  if (!ROOM) fail('缺少 ROOM');
  const browser = await chromium.launch({
    channel: 'msedge',
    headless: true,
    args: [
      '--no-sandbox',
      // 3061 为自签 WSS（RFC 7118）：headless 必须忽略证书错误（与 web-edge-e2e-run.js 同法）。
      '--ignore-certificate-errors',
    ],
  });
  const ctx = await browser.newContext({ ignoreHTTPSErrors: true, viewport: { width: 1280, height: 720 } });
  const page = await ctx.newPage();
  // 观测转发：失败原因必须落进本 stdout（ps1 侧落盘并 tail），否则只有空文件。
  page.on('console', m => console.log('[console]', m.text()));
  page.on('pageerror', e => console.error('[pageerror]', e.message));
  page.on('requestfailed', r => console.log('[requestfailed]', r.url(), r.failure() && r.failure().errorText));
  page.on('websocket', ws => { console.log('[ws open]', ws.url()); ws.on('close', () => console.log('[ws closed]')); });

  const url = 'http://127.0.0.1:' + PORT + '/sip-viewer.html?target=' + encodeURIComponent(ROOM)
    + '&device=' + encodeURIComponent(VIEWER) + '&signal=' + encodeURIComponent(SIGNAL)
    + (TOKEN ? '&token=' + encodeURIComponent(TOKEN) : '');

  const dumpPage = async (tag) => {
    try {
      const s = await page.evaluate(() => ({
        status: document.getElementById('status').innerText,
        ice: document.getElementById('ice').innerText,
        log: document.getElementById('log').innerText.slice(-2000),
        readyState: document.getElementById('video').readyState,
      }));
      console.log('---STATUS---'); console.log(s.status);
      console.log('---ICE---'); console.log(s.ice);
      console.log('---PAGELOG---'); console.log(s.log);
      console.log('---READYSTATE--- ' + s.readyState);
    } catch (_) { console.log('---PAGELOG--- (页面不可读: ' + tag + ')'); }
  };

  try {
    await page.goto(url);
    await page.click('#connect');

    // 等待媒体轨：video.readyState>=2（HAVE_CURRENT_DATA）= 解码出帧，比状态栏文案硬。
    let videoReady = false;
    try {
      await page.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 40000 });
      videoReady = true;
    } catch (_) { /* 失败现场由 dumpPage 落盘 */ }
    console.log('VIDEO_READY=' + videoReady);

    // 输入 data channel 必须 open 才能发；页面日志在 onopen 时打 "input channel open"。
    let inputOpen = false;
    try {
      await page.waitForFunction(() => document.getElementById('log').innerText.includes('input channel open'), { timeout: 20000 });
      inputOpen = true;
    } catch (_) {}
    console.log('INPUT_CHANNEL_OPEN=' + inputOpen);

    let inputSent = 0;
    if (inputOpen) {
      // 只发 mousemove（不发按键/点击）：publisher 侧会真实注入，最小化桌面副作用。
      // 坐标绕视频中心微动，保证 getBoundingClientRect 非零且事件有差异。
      for (let i = 0; i < 12; i++) {
        await page.$eval('#video', (v, i) => {
          const r = v.getBoundingClientRect();
          if (r.width === 0 || r.height === 0) return;
          const x = r.left + r.width * (0.4 + 0.02 * (i % 5));
          const y = r.top + r.height * (0.4 + 0.02 * (i % 3));
          v.dispatchEvent(new MouseEvent('mousemove', { clientX: x, clientY: y, bubbles: true }));
        }, i);
        inputSent++;
        await new Promise(r => setTimeout(r, 120));
      }
    }
    console.log('INPUT_SENT=' + inputSent);

    // 媒体轨与候选对：这是「浏览器收到什么」和「媒体走哪条路」的直接证据。
    const info = await page.evaluate(async () => {
      let pc = null;
      try { pc = (typeof rtc === 'undefined') ? null : rtc; } catch (_) { pc = null; }
      if (!pc) return { error: 'rtc 不可达' };
      const receivers = pc.getReceivers().map(r => ({ kind: r.track.kind, readyState: r.track.readyState, muted: r.track.muted }));
      const local = pc.localDescription ? pc.localDescription.sdp : '';
      const remote = pc.remoteDescription ? pc.remoteDescription.sdp : '';
      const mLines = (sdp) => (sdp.match(/^m=[^\r\n]*/gm) || []);
      const rtpmap = (sdp) => (sdp.match(/^a=rtpmap:[^\r\n]*/gm) || []);
      let pair = null, localC = null, remoteC = null, pairCount = 0;
      let dtlsState = null, inbound = [], codecs = [];
      try {
        const stats = await pc.getStats();
        const byId = new Map();
        let selectedId = null;
        stats.forEach(r => {
          byId.set(r.id, r);
          if (r.type === 'transport') { if (r.selectedCandidatePairId) selectedId = r.selectedCandidatePairId; if (r.dtlsState) dtlsState = r.dtlsState; }
          if (r.type === 'candidate-pair' && r.state === 'succeeded') pairCount++;
          if (r.type === 'codec') codecs.push({ mimeType: r.mimeType, payloadType: r.payloadType, clockRate: r.clockRate });
          if (r.type === 'inbound-rtp') inbound.push({ kind: r.kind, ssrc: r.ssrc, packetsReceived: r.packetsReceived, bytesReceived: r.bytesReceived, framesReceived: r.framesReceived, framesDecoded: r.framesDecoded, frameWidth: r.frameWidth, frameHeight: r.frameHeight, codecId: r.codecId });
        });
        // inbound-rtp 的 codecId → mimeType（判定协商到的编解码器是否与发布端帧格式一致）。
        stats.forEach(r => { if (r.type === 'inbound-rtp' && r.codecId && byId.get(r.codecId)) { const hit = inbound.find(x => x.ssrc === r.ssrc); if (hit) hit.mimeType = byId.get(r.codecId).mimeType; } });
        if (selectedId && byId.get(selectedId)) pair = byId.get(selectedId);
        if (!pair) stats.forEach(r => { if (!pair && r.type === 'candidate-pair' && r.state === 'succeeded' && r.nominated) pair = r; });
        if (!pair) stats.forEach(r => { if (!pair && r.type === 'candidate-pair' && r.state === 'succeeded') pair = r; });
        if (pair) {
          localC = byId.get(pair.localCandidateId) || null;
          remoteC = byId.get(pair.remoteCandidateId) || null;
        }
      } catch (e) { return { error: 'getStats 失败: ' + e.message, receivers }; }
      return {
        receivers,
        offerMLines: mLines(local),
        answerMLines: mLines(remote),
        answerRtpmap: rtpmap(remote),
        iceConnectionState: pc.iceConnectionState,
        dtlsState,
        inbound,
        codecs,
        pairCount,
        pairState: pair ? pair.state : null,
        nominated: pair ? !!pair.nominated : null,
        localCandidate: localC ? { type: localC.candidateType, ip: localC.address, port: localC.port, protocol: localC.protocol } : null,
        remoteCandidate: remoteC ? { type: remoteC.candidateType, ip: remoteC.address, port: remoteC.port, protocol: remoteC.protocol } : null,
        offerSdp: local,
        answerSdp: remote,
      };
    });
    console.log('RECEIVERS=' + JSON.stringify(info.receivers || []));
    console.log('OFFER_MLINES=' + JSON.stringify(info.offerMLines || []));
    console.log('ANSWER_MLINES=' + JSON.stringify(info.answerMLines || []));
    console.log('ANSWER_RTPMAP=' + JSON.stringify(info.answerRtpmap || []));
    console.log('ICE_STATE=' + (info.iceConnectionState || '') + ' DTLS_STATE=' + (info.dtlsState || ''));
    console.log('INBOUND_RTP=' + JSON.stringify(info.inbound || []));
    console.log('CODECS=' + JSON.stringify(info.codecs || []));
    if (process.env.DUMP_SDP) {
      console.log('---OFFER_SDP---'); console.log(info.offerSdp || '');
      console.log('---ANSWER_SDP---'); console.log(info.answerSdp || '');
    }
    console.log('PAIR_STATE=' + (info.pairState || '') + ' nominated=' + info.nominated + ' succeededPairs=' + info.pairCount);
    const rc = info.remoteCandidate;
    console.log('PEER_CANDIDATE=' + (rc ? [rc.type, rc.ip, rc.port, rc.protocol].join('|') : 'none'));
    console.log('PEER_PORT=' + (rc ? rc.port : ''));
    console.log('PEER_TYPE=' + (rc ? rc.type : ''));

    const audioTrack = (info.receivers || []).some(r => r.kind === 'audio');
    console.log('AUDIO_TRACK=' + (audioTrack ? 'present' : 'absent'));
    if (!audioTrack) {
      // 不是静默放过：sip-viewer.html 只建 recvonly video transceiver（无 audio m-line），
      // offer/answer 协商不出音频——这一条应在结论里如实呈现，而不是当作已验证。
      console.log('NOTE: sip-viewer.html 未建 audio transceiver，本方向不协商音频（见 ANSWER_MLINES）。');
    }

    const noRelay = !!rc && rc.type !== 'relay';
    console.log('NO_RELAY=' + noRelay);
    const ok = videoReady && inputOpen && inputSent > 0 && noRelay;
    console.log(ok ? 'RUNNER_PASS' : 'RUNNER_FAIL');
    await dumpPage('end');
    await browser.close();
    process.exit(ok ? 0 : 2);
  } catch (e) {
    console.error('E2E FAIL:', e.message);
    await dumpPage('exception');
    await browser.close().catch(() => {});
    process.exit(1);
  }
})().catch(e => fail(e.message));
