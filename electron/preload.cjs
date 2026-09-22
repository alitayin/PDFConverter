'use strict';

const { contextBridge, ipcRenderer, webUtils } = require('electron');

if (typeof document !== 'undefined') {
  const setPlatform = () => {
    document.documentElement.dataset.platform = process.platform;
  };
  if (document.documentElement) setPlatform();
  else document.addEventListener('DOMContentLoaded', setPlatform, { once: true });
}

async function call(command, args = {}) {
  const response = await ipcRenderer.invoke('desktop:command', command, args);
  if (!response?.ok) throw new Error(response?.error ?? 'DESKTOP_RUNTIME_UNAVAILABLE');
  return response.result;
}

contextBridge.exposeInMainWorld('desktop', {
  getSelfCheck: () => call('get_self_check'),
  getDiagnosticInfo: () => call('get_diagnostic_info'),
  inspectInputFiles: (paths) => call('inspect_input_files', { paths }),
  startJob: (request) => call('start_job', { request }),
  cancelJob: (jobId) => call('cancel_job', { jobId }),
  chooseInputFiles: () => call('choose_input_files'),
  chooseOutputDir: () => call('choose_output_dir'),
  openPath: (path) => call('open_path', { path }),
  copyText: (text) => call('copy_text', { text }),
  pathForFile: (file) => webUtils.getPathForFile(file),
  onProgress: (handler) => {
    const listener = (_event, payload) => handler(payload);
    ipcRenderer.on('desktop:progress', listener);
    return () => ipcRenderer.removeListener('desktop:progress', listener);
  },
  onEngineTerminated: (handler) => {
    const listener = () => handler();
    ipcRenderer.on('desktop:engine-terminated', listener);
    return () => ipcRenderer.removeListener('desktop:engine-terminated', listener);
  },
});
