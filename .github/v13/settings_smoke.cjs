const { chromium } = require('playwright-core');
const fs = require('node:fs');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

const edgeCandidates = [
  'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe',
  'C:/Program Files/Microsoft/Edge/Application/msedge.exe',
];
const executablePath = edgeCandidates.find((candidate) => fs.existsSync(candidate));
if (!executablePath) throw new Error('Microsoft Edge executable not found');

const htmlPath = path.resolve(process.argv[2]);
const outputDir = path.resolve(process.argv[3]);
fs.mkdirSync(outputDir, { recursive: true });

(async () => {
  const browser = await chromium.launch({ executablePath, headless: true });
  const errors = [];
  try {
    for (const viewport of [{ width: 640, height: 420 }, { width: 780, height: 560 }]) {
      const page = await browser.newPage({ viewport });
      page.on('pageerror', (error) => errors.push(`${viewport.width}x${viewport.height}: ${error.message}`));
      await page.addInitScript(() => {
        const listeners = new Map();
        Object.defineProperty(window, 'chrome', {
          configurable: true,
          value: {
            webview: {
              postMessage() {},
              addEventListener(name, callback) { listeners.set(name, callback); },
              removeEventListener(name) { listeners.delete(name); },
            },
          },
        });
      });
      await page.goto(pathToFileURL(htmlPath).href, { waitUntil: 'domcontentloaded' });
      await page.waitForTimeout(200);

      const result = await page.evaluate(() => {
        const select = document.getElementById('layout-monitor_overflow');
        const options = select ? Array.from(select.options).map((option) => option.value) : [];
        const navFailures = [];
        for (const item of document.querySelectorAll('.nav-item')) {
          if (!(item instanceof HTMLElement)) continue;
          item.click();
          if (!item.classList.contains('active')) {
            navFailures.push(item.textContent?.trim() || '<unnamed>');
          }
        }
        const visibleControlsOutside = Array.from(
          document.querySelectorAll('input, select, button, textarea')
        ).filter((element) => {
          const style = getComputedStyle(element);
          if (style.display === 'none' || style.visibility === 'hidden') return false;
          const rect = element.getBoundingClientRect();
          return rect.width > 0 && (rect.left < -1 || rect.right > innerWidth + 1);
        }).map((element) => element.id || element.getAttribute('aria-label') || element.tagName);
        return {
          selectPresent: Boolean(select),
          options,
          documentOverflow: document.documentElement.scrollWidth > innerWidth + 1,
          bodyOverflow: document.body.scrollWidth > innerWidth + 1,
          navFailures,
          visibleControlsOutside,
        };
      });

      if (!result.selectPresent) throw new Error('Monitor overflow select is missing');
      if (result.options.join(',') !== 'clip,hide') {
        throw new Error(`Unexpected monitor overflow options: ${result.options.join(',')}`);
      }
      if (result.documentOverflow || result.bodyOverflow) {
        throw new Error(`Horizontal page overflow at ${viewport.width}x${viewport.height}`);
      }
      if (result.navFailures.length) {
        throw new Error(`Navigation did not activate: ${result.navFailures.join(', ')}`);
      }
      if (result.visibleControlsOutside.length) {
        throw new Error(`Controls outside viewport: ${result.visibleControlsOutside.join(', ')}`);
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
  if (errors.length) throw new Error(`Settings page errors:\n${errors.join('\n')}`);
  console.log('Settings GUI smoke test passed');
})().catch((error) => {
  console.error(error);
  process.exit(1);
});
