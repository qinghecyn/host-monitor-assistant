import { chromium } from 'playwright';

const BASE = 'http://127.0.0.1:12317';
const OUT = 'D:/xiaozhushou_monitor/docs/shots';
const errors = [];

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 } });
const page = await ctx.newPage();
page.on('console', (m) => { if (m.type() === 'error') errors.push('[console] ' + m.text()); });
page.on('pageerror', (e) => errors.push('[pageerror] ' + e.message));

await page.goto(BASE + '/', { waitUntil: 'networkidle' });
await page.fill('#login-username', 'admin');
await page.fill('#login-password', 'admin123');
await page.click('#btn-login');
await page.waitForSelector('#main-view:not(.hidden)', { timeout: 10000 });
await page.waitForTimeout(4000);
await page.screenshot({ path: OUT + '/20-installed-dashboard.png' });

await page.click('#btn-servers');
await page.waitForSelector('#servers-panel:not(.hidden)');
await page.waitForTimeout(600);
await page.screenshot({ path: OUT + '/21-installed-servers.png' });

// 删除流程的二次确认长什么样
await page.click('.sv-card button[data-act="del"]');
await page.waitForTimeout(300);
await page.screenshot({ path: OUT + '/22-installed-delete-confirm.png' });
await page.click('.sv-card button[data-act="del-no"]');

// 设置面板
await page.click('#btn-sv-close');
await page.click('#btn-config');
await page.waitForTimeout(400);
await page.screenshot({ path: OUT + '/23-installed-settings.png' });

const info = {
  os: await page.textContent('#os-badge'),
  cpu: await page.textContent('#v-cpu'),
  mem: await page.textContent('#v-mem'),
  temp: await page.textContent('#v-temp'),
  disks: await page.$$eval('#disk-list li', (e) => e.length),
  errors,
};
console.log(JSON.stringify(info, null, 2));
await browser.close();
