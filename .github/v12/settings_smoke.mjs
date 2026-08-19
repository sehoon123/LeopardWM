import { pathToFileURL } from 'node:url';
import path from 'node:path';

const fs = await import('node:fs');
const playwrightRoot = process.env.NODE_PATH
  ? path.resolve(process.env.NODE_PATH)
  : path.resolve(process.cwd(), 'settings-smoke/node_modules');
const playwrightEntry = path.join(playwrightRoot, 'playwright-core', 'index.js');
if (!fs.existsSync(playwrightEntry)) {
  throw new Error(`playwright-core entry not found: ${playwrightEntry}`);
}
const { chromium } = await import(pathToFileURL(playwrightEntry).href);

const edgeCandidates = [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
];
const executablePath = edgeCandidates.find((candidate) => fs.existsSync(candidate));
if (!executablePath) throw new Error('Microsoft Edge executable not found');

const htmlPath = path.resolve(process.argv[2]);
const outputDir = path.resolve(process.argv[3]);
fs.mkdirSync(outputDir, { recursive: true });

const browser = await chromium.launch({ executablePath, headless: true });
const errors = [];
try {
  for (const viewport of [{ width: 640, height: 420 }, { width: 780, height: 560 }]) {
    const page = await browser.newPage({ viewport });
    page.on('pageerror', (error) => errors.push(`${viewport.width}x${viewport.height}: ${error.message}`));
    await page.addInitScript(() => {
      const listeners = new Map();
      window.chrome = {
        webview: {
          postMessage() {},
          addEventListener(name, callback) { listeners.set(name, callback); },
          removeEventListener(name) { listeners.delete(name); },
        },
      };
    });
    await page.goto(pathToFileURL(htmlPath).href, { waitUntil: 'domcontentloaded' });
    await page.waitForTimeout(150);

    const result = await page.evaluate(() => {
      const select = document.getElementById('layout-monitor_overflow');
      const options = select ? Array.from(select.options).map((option) => option.value) : [];
      const navItems = Array.from(document.querySelectorAll('.nav-item'));
      const navFailures = [];
      for (const item of navItems) {
        item.click();
        const active = document.querySelector('.section.active, .settings-section.active');
        if (!active) navFailures.push(item.textContent?.trim() || '<unnamed>');
      }
      return {
        selectPresent: Boolean(select),
        options,
        documentOverflow: document.documentElement.scrollWidth > window.innerWidth + 1,
        bodyOverflow: document.body.scrollWidth > window.innerWidth + 1,
        navFailures,
      };
    });

    if (!result.selectPresent) throw new Error('Monitor overflow select is missing');
    if (result.options.join(',') !== 'clip,hide') {
      throw new Error(`Unexpected monitor overflow options: ${result.options.join(',')}`);
    }
    if (result.documentOverflow || result.bodyOverflow) {
      throw new Error(`Horizontal overflow at ${viewport.width}x${viewport.height}`);
    }
    if (result.navFailures.length) {
      throw new Error(`Navigation failed: ${result.navFailures.join(', ')}`);
    }
    await page.screenshot({
      path: path.join(outputDir, `settings-${viewport.width}x${viewport.height}.png`),
      fullPage: true,
    });
    await page.close();
  }
} finally {
  await browser.close();
}

if (errors.length) {
  throw new Error(`Settings page errors:\n${errors.join('\n')}`);
}
console.log('Settings GUI smoke test passed');