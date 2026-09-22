import type { ConversionKind, ConversionOptions } from './conversion';

export type StartJobRequest = {
  kind: ConversionKind;
  inputs: string[];
  outputDir?: string;
  options: ConversionOptions;
};

export type DesktopProgress = {
  job_id: string;
  seq: number;
  state: 'queued' | 'running' | 'succeeded' | 'failed' | 'cancelled' | 'timed_out';
  completed: number;
  total: number;
  phase: string;
  message: string;
  outputs?: string[];
  error_code?: string;
};

export type InputFileInfo = { path: string; name: string; size: number };

export type SelfCheck = {
  status: 'ready' | 'repairable';
  checks: Array<{ id: string; status: 'passed' | 'failed'; message: string }>;
};

type DesktopAPI = {
  getSelfCheck(): Promise<SelfCheck>;
  getDiagnosticInfo(): Promise<string>;
  inspectInputFiles(paths: string[]): Promise<InputFileInfo[]>;
  startJob(request: StartJobRequest): Promise<string>;
  cancelJob(jobId: string): Promise<void>;
  chooseInputFiles(): Promise<string[]>;
  chooseOutputDir(): Promise<string | null>;
  openPath(path: string): Promise<void>;
  copyText(text: string): Promise<void>;
  pathForFile(file: File): string;
  onProgress(handler: (event: DesktopProgress) => void): () => void;
  onEngineTerminated(handler: () => void): () => void;
};

declare global {
  interface Window { desktop?: DesktopAPI }
}

export function isDesktopRuntime() {
  return typeof window !== 'undefined' && Boolean(window.desktop);
}

function desktop(): DesktopAPI {
  if (typeof window === 'undefined' || !window.desktop) throw new Error('DESKTOP_RUNTIME_REQUIRED');
  return window.desktop;
}

export function getSelfCheck() { return desktop().getSelfCheck(); }
export function getDiagnosticInfo() { return desktop().getDiagnosticInfo(); }
export function inspectInputFiles(paths: string[]) { return desktop().inspectInputFiles(paths); }
export function startJob(request: StartJobRequest) { return desktop().startJob(request); }
export function cancelJob(jobId: string) { return desktop().cancelJob(jobId); }
export function chooseInputFiles() { return desktop().chooseInputFiles(); }
export function chooseOutputDir() { return desktop().chooseOutputDir(); }
export function openPath(path: string) { return desktop().openPath(path); }
export function copyText(text: string) { return desktop().copyText(text); }
export function pathForFile(file: File) { return desktop().pathForFile(file); }

export async function listenToProgress(handler: (event: DesktopProgress) => void) {
  return isDesktopRuntime() ? desktop().onProgress(handler) : () => undefined;
}

export function listenToEngineTermination(handler: () => void) {
  return isDesktopRuntime() ? desktop().onEngineTerminated(handler) : () => undefined;
}
