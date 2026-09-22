'use strict';

const { app, BrowserWindow, clipboard, dialog, ipcMain, nativeImage, protocol, session, shell } = require('electron');
const { existsSync, mkdirSync, statSync } = require('node:fs');
const { dirname, isAbsolute, join, resolve } = require('node:path');
const { RustBridge } = require('./bridge.cjs');
const { officeBridgeEnv } = require('./office-runtime.cjs');
const { inputExtensions } = require('./input-extensions.cjs');
const { serveStatic } = require('./static.cjs');
const { renderLocalHtmlToPdf } = require('./offline-print.cjs');

app.setName('Ayst Arc PDF');
protocol.registerSchemesAsPrivileged([{ scheme: 'app', privileges: { standard: true, secure: true, supportFetchAPI: true } }]);

let mainWindow;
let engine;
let dialogOpen = false;
let shuttingDown = false;
let engineReady = false;
let rendererReady = false;
let desktopReadyReported = false;
const allowedOutputs = new Set();

function reportDesktopReady() {
  if (!desktopReadyReported && engineReady && rendererReady) {
    desktopReadyReported = true;
    console.log('[desktop] ready');
  }
}

function setDockIcon() {
  if (process.platform !== 'darwin' || !app.dock?.setIcon) return;
  const asset = app.isPackaged
    ? join(app.getAppPath(), 'out', 'ayst-arc-mark.png')
    : join(app.getAppPath(), 'public', 'ayst-arc-mark.png');
  if (!existsSync(asset)) return;
  const image = nativeImage.createFromPath(asset);
  if (!image.isEmpty()) app.dock.setIcon(image);
}

function enginePath() {
  if (app.isPackaged) return join(process.resourcesPath, 'bin', process.platform === 'win32' ? 'minimal-pdf-converter.exe' : 'minimal-pdf-converter');
  const root = join(__dirname, '..');
  const binary = process.platform === 'win32' ? 'minimal-pdf-converter.exe' : 'minimal-pdf-converter';
  return process.env.MINIMALPDF_RUST_ENGINE || join(root, 'rust-engine', 'target', 'debug', binary);
}

function workerPath() {
  const binary = process.platform === 'win32' ? 'pdf_to_txt_worker.exe' : 'pdf_to_txt_worker';
  const base = app.isPackaged ? join(process.resourcesPath, 'bin') : join(__dirname, '..', 'rust-engine', 'target', 'release');
  const target = join(base, binary);
  return existsSync(target) ? target : null;
}

function startEngine() {
  const executable = enginePath();
  if (!existsSync(executable)) {
    console.error('[desktop] engine executable is missing');
    return;
  }
  const args = [];
  const worker = workerPath();
  if (worker) args.push('--worker-path', worker);
  const env = officeBridgeEnv({ isPackaged: app.isPackaged, resourcesPath: process.resourcesPath, platform: process.platform });
  env.MINIMALPDF_ELECTRON_EXECUTABLE = process.execPath;
  env.MINIMALPDF_ELECTRON_APP_ROOT = app.isPackaged ? '' : app.getAppPath();
  const bridge = new RustBridge(executable, { args, env });
  engine = bridge;
  void bridge.invoke('get_self_check').then((check) => {
    if (engine !== bridge) return;
    if (!check || check.status !== 'ready') {
      console.error('[desktop] engine self-check did not pass');
      return;
    }
    engineReady = true;
    reportDesktopReady();
  }).catch(() => {
    console.error('[desktop] engine self-check failed');
  });
  bridge.on('progress', (progress) => {
    if (progress.state === 'succeeded' && Array.isArray(progress.outputs)) {
      for (const output of progress.outputs) {
        if (typeof output === 'string' && isAbsolute(output)) {
          allowedOutputs.add(resolve(output));
          allowedOutputs.add(dirname(resolve(output)));
        }
      }
    }
    if (mainWindow && !mainWindow.isDestroyed()) mainWindow.webContents.send('desktop:progress', progress);
  });
  bridge.on('terminated', () => {
    if (engine === bridge) {
      engine = undefined;
      engineReady = false;
    }
    if (!shuttingDown && mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send('desktop:engine-terminated');
    }
  });
}

function validPaths(paths) {
  return Array.isArray(paths) && paths.length > 0 && paths.length <= 200
    && paths.every((path) => typeof path === 'string' && path.length <= 4096 && isAbsolute(path));
}

async function selectFiles() {
  if (dialogOpen) throw new Error('DIALOG_BUSY');
  dialogOpen = true;
  try {
    const { canceled, filePaths } = await dialog.showOpenDialog(mainWindow, {
      title: 'Choose files',
      properties: ['openFile', 'multiSelections'],
      filters: [{ name: 'Supported files', extensions: inputExtensions }],
    });
    return canceled ? [] : filePaths;
  } finally {
    dialogOpen = false;
  }
}

async function selectDirectory() {
  if (dialogOpen) throw new Error('DIALOG_BUSY');
  dialogOpen = true;
  try {
    const { canceled, filePaths } = await dialog.showOpenDialog(mainWindow, {
      title: 'Choose output folder',
      properties: ['openDirectory', 'createDirectory'],
    });
    return canceled ? null : (filePaths[0] ?? null);
  } finally {
    dialogOpen = false;
  }
}

async function desktopCommand(command, args) {
  if (!args || typeof args !== 'object' || Array.isArray(args)) throw new Error('INVALID_REQUEST');
  if (command === 'choose_input_files') return selectFiles();
  if (command === 'choose_output_dir') return selectDirectory();
  if (command === 'copy_text') {
    if (typeof args.text !== 'string' || args.text.length > 16_384) throw new Error('INVALID_REQUEST');
    clipboard.writeText(args.text);
    return null;
  }
  if (command === 'open_path') {
    if (typeof args.path !== 'string' || !isAbsolute(args.path)) throw new Error('PATH_NOT_ALLOWED');
    const target = resolve(args.path);
    if (!allowedOutputs.has(target) || !existsSync(target) || (!statSync(target).isFile() && !statSync(target).isDirectory())) {
      throw new Error('PATH_NOT_ALLOWED');
    }
    if (await shell.openPath(target)) throw new Error('OPEN_FAILED');
    return null;
  }
  if (command === 'inspect_input_files' && !validPaths(args.paths)) throw new Error('INVALID_REQUEST');
  if (command === 'start_job' && (!args.request || !validPaths(args.request.inputs))) throw new Error('INVALID_REQUEST');
  if (command === 'cancel_job' && (typeof args.jobId !== 'string' || args.jobId.length > 128)) throw new Error('INVALID_REQUEST');
  if (!engine) throw new Error('ENGINE_NOT_INSTALLED');
  return engine.invoke(command, args);
}

function createWindow() {
  mainWindow = new BrowserWindow({
    title: 'Ayst Arc PDF',
    width: 1080,
    height: 760,
    minWidth: 720,
    minHeight: 560,
    backgroundColor: '#f8f8f6',
    ...(process.platform === 'darwin' ? {
      titleBarStyle: 'hiddenInset',
      trafficLightPosition: { x: 16, y: 18 },
    } : {}),
    webPreferences: {
      preload: join(__dirname, 'preload.cjs'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      webSecurity: true,
    },
  });
  mainWindow.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  mainWindow.webContents.on('will-navigate', (event) => event.preventDefault());
  mainWindow.webContents.on('did-fail-load', (_event, code, description, url, isMainFrame) => {
    console.error('[desktop] load failed', { code, description, url, isMainFrame });
  });
  mainWindow.webContents.on('render-process-gone', (_event, details) => {
    console.error('[desktop] renderer exited', details.reason);
  });
  mainWindow.webContents.once('did-finish-load', () => {
    rendererReady = true;
    reportDesktopReady();
  });
  mainWindow.webContents.on('console-message', (_event, details) => {
    if (details.level === 'error') console.error('[desktop] renderer error', details.message);
  });
  void mainWindow.loadURL('app://local/').catch((error) => {
    console.error('[desktop] could not load workspace', error);
  });
  mainWindow.on('closed', () => {
    mainWindow = undefined;
    rendererReady = false;
  });
}

if (process.argv.includes('--html-print-worker')) {
  const index = process.argv.indexOf('--html-print-worker');
  const inputPath = process.argv[index + 1];
  const outputPath = process.argv[index + 2];
  if (process.argv.length !== index + 3 || !isAbsolute(inputPath ?? '') || !isAbsolute(outputPath ?? '')) {
    app.exit(1);
  } else {
    const profile = join(dirname(outputPath), 'electron-profile');
    mkdirSync(profile, { recursive: true, mode: 0o700 });
    app.setPath('userData', profile);
    app.whenReady().then(() => renderLocalHtmlToPdf({ inputPath, outputPath, timeoutMs: 90_000 }))
      .then(() => app.quit()).catch(() => app.exit(1));
  }
} else {
if (!app.isPackaged) {
  const devUserData = `${app.getPath('userData')}-dev`;
  mkdirSync(devUserData, { recursive: true, mode: 0o700 });
  app.setPath('userData', devUserData);
}

if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (!mainWindow) return;
    if (mainWindow.isMinimized()) mainWindow.restore();
    mainWindow.focus();
  });

  app.whenReady().then(() => {
    protocol.handle('app', (request) => serveStatic(join(app.getAppPath(), 'out'), request.url));
    session.defaultSession.setPermissionRequestHandler((_webContents, _permission, callback) => callback(false));
    session.defaultSession.webRequest.onBeforeRequest(
      { urls: ['http://*/*', 'https://*/*', 'file://*/*'] },
      (_details, callback) => callback({ cancel: true }),
    );
    app.on('web-contents-created', (_event, contents) => {
      contents.on('will-attach-webview', (event) => event.preventDefault());
    });
    setDockIcon();
    startEngine();
    ipcMain.handle('desktop:command', async (event, command, args) => {
      if (!mainWindow || event.sender !== mainWindow.webContents || event.senderFrame !== mainWindow.webContents.mainFrame) {
        return { ok: false, error: 'INVALID_ORIGIN' };
      }
      try {
        return { ok: true, result: await desktopCommand(command, args) };
      } catch (error) {
        return { ok: false, error: error instanceof Error ? error.message.slice(0, 300) : 'DESKTOP_RUNTIME_UNAVAILABLE' };
      }
    });
    createWindow();
    app.on('activate', () => { if (BrowserWindow.getAllWindows().length === 0) createWindow(); });
  });
  app.on('before-quit', (event) => {
    if (shuttingDown) return;
    shuttingDown = true;
    if (engine) {
      event.preventDefault();
      void engine.close().finally(() => app.quit());
    }
  });
  app.on('window-all-closed', () => { if (process.platform !== 'darwin') app.quit(); });
}
}
