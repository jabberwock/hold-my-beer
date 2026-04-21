#!/usr/bin/env node
// Post-minify step: inline public/app.min.css into public/index.html between
// <!--inline:css-start--> and <!--inline:css-end--> markers, and strip the
// now-redundant <link rel=stylesheet href=app.min.css>. Keeps the critical
// stylesheet in the HTML document itself so FCP doesn't wait on a separate
// network round-trip — Lighthouse perf audit cares about this on simulated
// 3G throttling.
import { readFileSync, writeFileSync } from 'node:fs';
const html = readFileSync('public/index.html', 'utf8');
const css  = readFileSync('public/app.min.css', 'utf8');

const startMarker = '<!--inline:css-start-->';
const endMarker   = '<!--inline:css-end-->';
const startIdx = html.indexOf(startMarker);
const endIdx   = html.indexOf(endMarker);
if (startIdx < 0 || endIdx < 0) {
  console.error('Missing CSS inline markers in public/index.html');
  process.exit(1);
}

const before = html.slice(0, startIdx + startMarker.length);
const after  = html.slice(endIdx);
const next   = `${before}<style>${css}</style>${after}`;

writeFileSync('public/index.html', next);
console.log(`Inlined ${css.length} bytes of CSS into index.html`);
