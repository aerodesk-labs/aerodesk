const { chromium } = require('playwright-core');
const ROOM = process.argv[2];
(async () => {
  const browser = await chromium.launch({ channel: 'msedge', headless: true, args: ['--no-sandbox'] });
  const page = await browser.newPage();
  page.on('pageerror', e => console.log('pageerror: ' + e.message));
  await page.goto(`http://127.0.0.1:3002/?room=${ROOM}&role=viewer&signal=ws://127.0.0.1:3003/ws`);
  await page.click('#connect');
  let videoReady = false;
  try {
    await page.waitForFunction(() => document.getElementById('video').readyState >= 2, { timeout: 30000 });
    videoReady = true;
  } catch (e) {}
  console.log('VIDEO_READY=' + videoReady);
  if (videoReady) {
    for (let i = 0; i < 10; i++) {
      await page.$eval('#video', v => {
        const r = v.getBoundingClientRect();
        v.dispatchEvent(new MouseEvent('mousemove', { clientX: r.left + r.width / 2, clientY: r.top + r.height / 2, bubbles: true }));
      });
      await new Promise(r => setTimeout(r, 100));
    }
    await new Promise(r => setTimeout(r, 1500));
  }
  const logText = await page.evaluate(() => document.getElementById('log').innerText).catch(() => '');
  console.log('---LOG---');
  console.log(logText.slice(-2000));
  await browser.close();
  process.exit(videoReady ? 0 : 2);
})().catch(e => { console.error('E2E FAIL:', e.message); process.exit(1); });
