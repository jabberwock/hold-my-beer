import { test, expect } from '@playwright/test';

test('design-smoke: theme toggle, a11y panel, mic, glance', async ({ page }) => {
  const errs = [];
  page.on('pageerror', e => errs.push('PAGE: ' + e.message));
  page.on('console',   m => { if (m.type() === 'error') errs.push('CONSOLE: ' + m.text()); });

  await page.goto('http://localhost:1421/');
  await page.waitForLoadState('networkidle');
  await page.evaluate(() => {
    document.getElementById('wizard').hidden = true;
    document.getElementById('dashboard').hidden = false;
  });

  // Theme toggle cycles saloon → daylight → saloon
  const t0 = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  await page.locator('#btn-toggle-theme').click();
  const t1 = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  await page.locator('#btn-toggle-theme').click();
  const t2 = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  expect(t0).toBe('saloon');
  expect(t1).toBe('daylight');
  expect(t2).toBe('saloon');

  // A11y panel opens
  await page.locator('#btn-toggle-a11y').click();
  await expect(page.locator('#a11y-panel')).toBeVisible();

  // Dyslexic font applies via CSS variable
  await page.locator('[data-pref="font"] .a11y-font-opt[data-val="dyslexic"]').click();
  const sans = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--font-sans'));
  expect(sans).toContain('OpenDyslexic');

  // Theme segmented control sets data-theme
  await page.locator('[data-pref="theme"] .a11y-seg-opt[data-val="daylight"]').click();
  await expect(page.locator('html')).toHaveAttribute('data-theme', 'daylight');

  // Text size slider updates --app-size
  await page.locator('#a11y-size').fill('18');
  await page.locator('#a11y-size').dispatchEvent('input');
  const size = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue('--app-size').trim());
  expect(size).toBe('18px');

  // Accent override
  await page.locator('.a11y-accent.acc-hop').click();
  const accent = await page.evaluate(() => document.documentElement.style.getPropertyValue('--accent'));
  expect(accent).toBe('#8bbf5f');

  // Roster panel toggle via a11y switches — flip off then on to verify both
  // directions and leave it visible for the glance-open step below.
  await page.locator('.a11y-toggle[data-toggle="roster"]').click();
  await expect(page.locator('#roster')).toHaveClass(/collapsed/);
  await page.locator('.a11y-toggle[data-toggle="roster"]').click();
  await expect(page.locator('#roster')).not.toHaveClass(/collapsed/);

  // Mic button is present (Chromium exposes SpeechRecognition so it's
  // enabled there; we only assert the control exists with a sane title).
  await expect(page.locator('#btn-mic')).toBeVisible();
  await expect(page.locator('#btn-mic')).toHaveAttribute('title', /voice|mic|listen/i);

  // Worker glance opens from a roster-item click
  await page.evaluate(() => {
    const list = document.getElementById('roster-list');
    list.innerHTML = '';
    const item = document.createElement('div');
    item.className = 'roster-item';
    item.dataset.worker = 'd4webdev';
    item.textContent = 'd4webdev';
    item.addEventListener('click', () => openWorkerGlance('d4webdev', 'Frontend'));
    list.appendChild(item);
  });
  await page.locator('.roster-item[data-worker="d4webdev"]').click();
  await expect(page.locator('#worker-glance')).toBeVisible();
  await expect(page.locator('#glance-name')).toHaveText('d4webdev');
  const barCount = await page.locator('#glance-sparkline .glance-spark-bar').count();
  expect(barCount).toBe(30);
  const uncommitted = Number(await page.locator('#glance-uncommitted').textContent());
  expect(uncommitted).toBeGreaterThanOrEqual(1);

  // Resize handle exists with a hittable width
  const handleW = await page.locator('#resize-roster').evaluate(el => el.getBoundingClientRect().width);
  expect(handleW).toBeGreaterThan(0);

  // Prefs persisted
  const saved = await page.evaluate(() => JSON.parse(localStorage.getItem('hmb.prefs')));
  expect(saved.v).toBe(1);
  expect(saved.font).toBe('dyslexic');
  expect(saved.theme).toBe('daylight');
  expect(saved.accent).toBe('#8bbf5f');

  // Filter the expected favicon 404 — dev-server doesn't ship one and the
  // existing wizard.spec.mjs has the same carve-out.
  const real = errs.filter(e => !/favicon|404/i.test(e));
  expect(real).toEqual([]);
});
