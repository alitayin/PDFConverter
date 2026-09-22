'use strict';

const { app } = require('electron');
const { mkdirSync } = require('node:fs');
const { dirname, isAbsolute, join } = require('node:path');
const { renderLocalHtmlToPdf } = require('./offline-print.cjs');

const [, , inputPath, outputPath, mode = 'success'] = process.argv;
if (isAbsolute(inputPath)) {
  const userData = join(dirname(inputPath), '.electron-fixture-profile');
  mkdirSync(userData, { recursive: true, mode: 0o700 });
  app.setPath('userData', userData);
}

app.whenReady().then(async () => {
  if (mode === 'oversized-output' || mode === 'invalid-output') {
    app.on('browser-window-created', (_event, window) => {
      window.webContents.printToPDF = async () => mode === 'oversized-output'
        ? Buffer.alloc(64 * 1024 * 1024 + 1)
        : Buffer.from('not a PDF');
    });
  }
  const options = { inputPath, outputPath };
  if (mode === 'timeout') options.timeoutMs = 1;
  if (mode === 'abort-before') {
    const controller = new AbortController();
    controller.abort();
    options.signal = controller.signal;
  }
  if (mode === 'abort-during') {
    const controller = new AbortController();
    options.signal = controller.signal;
    setTimeout(() => controller.abort(), 10);
  }
  if (mode === 'retry') {
    const controller = new AbortController();
    controller.abort();
    try {
      await renderLocalHtmlToPdf({ ...options, signal: controller.signal });
      throw new Error('HTML_PRINT_CANCEL_NOT_ENFORCED');
    } catch (error) {
      if (error.message !== 'HTML_PRINT_CANCELLED') throw error;
    }
  }
  const result = await renderLocalHtmlToPdf(options);
  console.log(`HTML_PRINT_RESULT=${result}`);
  app.quit();
}).catch((error) => {
  console.error(`HTML_PRINT_ERROR=${error.message}`);
  app.exit(1);
});
