import { chromium } from 'playwright';

const BASE = 'http://127.0.0.1:8899';
const OUT = 'D:/xiaozhushou_monitor/docs/shots';
const errors = [];

const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1360, height: 900 }, deviceScaleFactor: 1 });
const page = await ctx.newPage();
page.on('console', (m) => { if (m.type() === 'error') errors.push('[console] ' + m.text()); });
page.on('pageerror', (e) => errors.push('[pageerror] ' + e.message));

await page.goto(BASE + '/', { waitUntil: 'networkidle' });
await page.screenshot({ path: OUT + '/01-login.png' });

// 登录
await page.fill('#login-username', 'admin');
await page.fill('#login-password', 'admin123');
await page.click('#btn-login');
await page.waitForSelector('#main-view:not(.hidden)', { timeout: 8000 });
await page.waitForTimeout(2500);
await page.screenshot({ path: OUT + '/02-dashboard.png' });

// 打开服务器管理
await page.click('#btn-servers');
await page.waitForSelector('#servers-panel:not(.hidden)');
await page.waitForTimeout(600);
await page.screenshot({ path: OUT + '/03-servers.png' });

// 点“添加服务器”，填一台新机器
await page.click('#btn-sv-add');
await page.fill('#sv-name', 'vps');
await page.fill('#sv-label', '香港小机');
await page.fill('#sv-host', '10.2.0.77');
await page.fill('#sv-port', '2222');
await page.fill('#sv-user', 'ubuntu');
await page.fill('#sv-pass', 'secret');
await page.fill('#sv-note', '测试机');
await page.waitForTimeout(200);
await page.screenshot({ path: OUT + '/04-server-form.png' });

// 测试连接（mock 模式会成功）
await page.click('#btn-sv-test');
await page.waitForSelector('#sv-test:not(.hidden)', { timeout: 8000 });
const testText = await page.textContent('#sv-test');
await page.screenshot({ path: OUT + '/05-test-ok.png' });

// 保存
await page.click('#btn-sv-save');
await page.waitForTimeout(1500);
await page.screenshot({ path: OUT + '/06-after-add.png' });

// 关掉抽屉看主界面是否多了一台
await page.click('#btn-sv-close');
await page.waitForTimeout(800);
const options = await page.$$eval('#target-select option', (els) => els.map((e) => e.textContent));

// 打开设置面板
await page.click('#btn-config');
await page.waitForTimeout(600);
await page.screenshot({ path: OUT + '/07-settings.png' });
await page.click('#btn-cfg-close');

// 手机竖屏
const mob = await ctx.newPage();
await mob.setViewportSize({ width: 390, height: 844 });
await mob.goto(BASE + '/', { waitUntil: 'networkidle' });
await mob.fill('#login-username', 'admin');
await mob.fill('#login-password', 'admin123');
await mob.click('#btn-login');
await mob.waitForSelector('#main-view:not(.hidden)');
await mob.waitForTimeout(2000);
await mob.screenshot({ path: OUT + '/08-mobile.png' });

console.log(JSON.stringify({ testText: testText.trim(), options, errors }, null, 2));
await browser.close();
