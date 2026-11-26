import { chromium } from 'playwright';

const BASE = 'http://127.0.0.1:8902';
const OUT = 'D:/xiaozhushou_monitor/docs/shots';
const errors = [];

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 } });
const page = await ctx.newPage();
page.on('console', (m) => { if (m.type() === 'error') errors.push('[console] ' + m.text()); });
page.on('pageerror', (e) => errors.push('[pageerror] ' + e.message));

await page.goto(BASE + '/', { waitUntil: 'networkidle' });
await page.fill('#login-username', 'root');
await page.fill('#login-password', 'woshinidie101');
await page.click('#btn-login');
await page.waitForSelector('#main-view:not(.hidden)', { timeout: 10000 });
await page.waitForTimeout(6000);
await page.screenshot({ path: OUT + '/10-real-dashboard.png' });

await page.click('#btn-servers');
await page.waitForSelector('#servers-panel:not(.hidden)');
await page.waitForTimeout(500);
await page.screenshot({ path: OUT + '/11-real-servers.png' });

const net = await page.textContent('#net-ifaces');
const rx = await page.textContent('#v-rx');
const diskCount = await page.$$eval('#disk-list li', (els) => els.length);
const os = await page.textContent('#os-badge');
const cpu = await page.textContent('#v-cpu');

console.log(JSON.stringify({ os, cpu, rx, net, diskCount, errors }, null, 2));
await browser.close();
