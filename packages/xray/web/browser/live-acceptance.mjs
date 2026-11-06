import { chromium } from 'playwright';
import assert from 'node:assert/strict';

// The launcher supplies these only to this isolated acceptance process. Never print them.
const browser = await chromium.launch({ headless: true, ...(process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE ? { executablePath: process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE } : {}) });
try {
  const page = await browser.newPage();
  const failures = [];
  page.on('pageerror', error => failures.push(error.message));
  await page.goto(process.env.XRAY_ACCEPTANCE_URL);
  if (process.env.XRAY_ACCEPTANCE_CODE) {
    await page.getByLabel('CLI pairing code').fill(process.env.XRAY_ACCEPTANCE_CODE);
    await page.getByRole('button', { name: 'Connect live' }).click();
  }
  await page.getByText('ready', { exact: true }).first().waitFor();
  assert.equal(new URL(page.url()).hash, '');
  const storage = await page.evaluate(() => JSON.stringify(localStorage));
  assert(!storage.includes(process.env.XRAY_ACCEPTANCE_CODE || 'not-a-real-secret'));
  await page.getByRole('button', { name: 'Stop observation', exact: true }).click();
  await page.getByText('finalized', { exact: true }).first().waitFor();
  assert.deepEqual(failures, []);
  console.log('Launcher -> browser -> gateway observation ready and finalized: PASS');
} finally {
  await browser.close();
}
