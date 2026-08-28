const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({
    channel: 'msedge', headless: true,
    args: [
      '--no-sandbox',
      '--use-fake-ui-for-media-stream',   // getDisplayMedia 免交互授权
      '--use-fake-device-for-media-stream', // fake 摄像头/屏幕源
      '--auto-accept-this-tab-capture',   // headless 屏幕共享
      '--enable-usermedia-screen-capturing',
      '--ignore-certificate-errors',      // 3061 为自签 WSS（RFC 7118）
    ],
  });
  // #598 P2a：JSON WSS 房间面退役——双浏览器 SIP-WSS 闭环
  // （sip-publisher.html UAS + sip-viewer.html UAC）。
  const WEB_SERVE_PORT = process.env.WEB_SERVE_PORT || 38083;
  const pub = await browser.newPage();
  pub.on('pageerror', e => console.log('pub pageerror: ' + e.message));
  await pub.goto(`http://127.0.0.1:${WEB_SERVE_PORT}/sip-publisher.html?device=${ROOM}&signal=wss://127.0.0.1:3061`);
  await pub.click('#connect');
  let pubReady = false;
  try {
    await pub.waitForFunction(() => document.getElementById('status').innerText.includes('等待观看端拨入'), { timeout: 30000 });
    pubReady = true;
  } catch (e) {}
  console.log('PUBLISHER_READY=' + pubReady);
  if (!pubReady) {
    const pubLog = await pub.evaluate(() => document.getElementById('log').innerText).catch(() => '');
    console.log('---PUBLOG---');
    console.log((pubLog || '').slice(-2000));
    await browser.close();
    process.exit(2);
  }
  // 观看页（呼入收流 + 输入事件经 data channel 直达被控页）
  const view = await browser.newPage();
  view.on('pageerror', e => console.log('view pageerror: ' + e.message));
  await view.goto(`http://127.0.0.1:${WEB_SERVE_PORT}/sip-viewer.html?target=${ROOM}&signal=wss://127.0.0.1:3061`);
  await view.click('#connect');
  let videoReady = false;
  try {
    await view.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 30000 });
    videoReady = true;
  } catch (e) {}
  console.log('VIDEO_READY=' + videoReady);
  if (videoReady) {
    for (let i = 0; i < 10; i++) {
      await view.$eval('#video', v => {
        const r = v.getBoundingClientRect();
        v.dispatchEvent(new MouseEvent('mousemove', { clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, bubbles: true }));
      });
      await new Promise(r => setTimeout(r, 100));
    }
    // 被控页收到输入事件（sip-publisher.html ondatachannel 记 "input: {...}"）
    let inputRelayed = false;
    try {
      await pub.waitForFunction(() => document.getElementById('log').innerText.includes('input: '), { timeout: 15000 });
      inputRelayed = true;
    } catch (e) {}
    console.log('INPUT_RELAYED=' + inputRelayed);
    await new Promise(r => setTimeout(r, 1500));
    const logText = await view.evaluate(() => document.getElementById('log').innerText).catch(() => '');
    console.log('---LOG---');
    console.log((logText || '').slice(-2000));
    await browser.close();
    process.exit(videoReady && inputRelayed ? 0 : 2);
  }
  const logText = await view.evaluate(() => document.getElementById('log').innerText).catch(() => '');
  console.log('---LOG---');
  console.log((logText || '').slice(-2000));
  await browser.close();
  process.exit(2);
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
