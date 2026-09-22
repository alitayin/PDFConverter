'use client';

import { useEffect, useMemo, useRef, useState } from 'react';
import type { ChangeEvent, DragEvent } from 'react';
import { ArrowRight, Check, Copy as CopyIcon, ExternalLink, FileArchive, FolderOpen, Images, Info, LoaderCircle, Minus, MousePointer2, Presentation, RotateCcw, Settings2, Trash2, Upload, X } from 'lucide-react';
import { accepts, acceptsName, conversionGroups, conversionModes, ConversionFile, ConversionGroup, ConversionKind, ConversionOptions, inputAccept, outputName } from '@/lib/conversion';
import { jobHasFinished, projectEngineTermination, projectJobProgress } from '@/lib/job-progress';
import { cancelJob, chooseInputFiles, chooseOutputDir, copyText, getDiagnosticInfo, getSelfCheck, inspectInputFiles, isDesktopRuntime, listenToEngineTermination, listenToProgress, openPath, pathForFile, startJob, type DesktopProgress, type SelfCheck } from '@/lib/desktop';
import { cn } from '@/lib/utils';

function formatSize(bytes: number) {
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function fileStatusLabel(file: ConversionFile) {
  if (file.state === 'running') return 'Processing';
  if (file.state === 'succeeded') return 'Done';
  if (file.state === 'failed') return 'Failed';
  if (file.state === 'cancelled') return 'Cancelled';
  if (file.state === 'timed_out') return 'Timed out';
  return 'Queued';
}

function rejectedFileNotice(kind: ConversionKind, count: number) {
  if (!count) return '';
  return kind === 'doc_to_pdf' || kind === 'docx_to_pdf'
    ? `${count} file(s) skipped: choose DOC or DOCX files.`
    : kind === 'odt_to_pdf'
    ? `${count} file(s) skipped: ODT only.`
    : kind === 'html_to_pdf'
    ? `${count} file(s) skipped: local HTML or HTM only.`
    : kind === 'txt_to_pdf' || kind === 'rtf_to_pdf'
    ? `${count} file(s) skipped: check the text document extension.`
    : kind === 'ppt_to_pdf' || kind === 'pptx_to_pdf'
    ? `${count} file(s) skipped: choose PPT or PPTX files.`
    : kind === 'odp_to_pdf'
    ? `${count} file(s) skipped: ODP only.`
    : kind === 'xlsx_to_pdf' || kind === 'ods_to_pdf'
    ? `${count} file(s) skipped: check the spreadsheet extension.`
    : kind === 'image_to_pdf'
    ? `${count} file(s) skipped: static PNG, JPG/JPEG, BMP, GIF, WebP or TIFF only.`
    : kind === 'svg_to_pdf'
    ? `${count} file(s) skipped: static SVG only.`
    : `${count} file(s) do not match this conversion.`;
}

const startJobErrorMessages: Record<string, string> = {
  NO_INPUTS: 'Choose at least one file.',
  INPUT_PATH_REQUIRED: 'Could not resolve the input path. Choose the file again.',
  INPUT_NOT_FOUND: 'The input file no longer exists. Choose it again.',
  INPUT_TOO_LARGE: 'The input exceeds the size limit. Remove it and retry.',
  OUTPUT_SIZE_EXCEEDED: 'The output exceeds the task limit. Reduce pages or quality.',
  PAGE_LIMIT_EXCEEDED: 'Too many pages selected. Reduce the page range.',
  PDF_TEXT_FIDELITY_UNSAFE: 'The PDF contains characters that cannot be mapped safely. Conversion stopped.',
  UNSUPPORTED_FORMAT: 'This file format is not supported for the selected conversion.',
  INVALID_DPI: 'DPI must be 150, 200 or 300.',
  INVALID_JPG_QUALITY: 'JPG quality must be between 1 and 100.',
  INVALID_PAGES: 'Invalid page range. Use “All” or a range such as 1-3,8.',
  OUTPUT_PATH_REQUIRED: 'The output directory must be an absolute path.',
  OUTPUT_DIR_NOT_FOUND: 'The output directory is missing or inaccessible.',
  OUTPUT_PATH_NOT_ALLOWED: 'This output directory is not writable.',
  SELF_CHECK_FAILED: 'The local engine self-check failed.'
};

function startJobErrorMessage(error: unknown, kind: ConversionKind) {
  const detail = error instanceof Error ? error.message : String(error);
  const code = /^([A-Z][A-Z0-9_]+):/.exec(detail)?.[1];
  return (code && startJobErrorMessages[code]) || 'The local conversion engine is unavailable.';
}

function formatToken(value: string) {
  const token = value.split(/\s*\/\s*|\s+·\s+/)[0].trim().toUpperCase();
  if (token === 'IMAGES' || token === 'IMAGE') return 'IMG';
  if (token === 'HTML') return 'HTML';
  if (token === 'MARKDOWN') return 'MD';
  return token;
}

function formatTone(value: string) {
  const token = formatToken(value);
  if (token === 'PDF') return 'pdf';
  if (token === 'DOC' || token === 'DOCX' || token === 'ODT' || token === 'RTF') return 'doc';
  if (token === 'PPT' || token === 'PPTX' || token === 'ODP') return 'slides';
  if (token === 'XLSX' || token === 'ODS' || token === 'CSV') return 'sheet';
  if (token === 'IMG' || token === 'SVG' || /^(PNG|JPG|JPEG|BMP|GIF|WEBP|TIF|TIFF)$/.test(token)) return 'image';
  return 'text';
}

function FormatIllustration({ value, compact = false }: { value: string; compact?: boolean }) {
  const label = formatToken(value);
  return (
    <div className={cn('format-illustration', compact && 'compact')} aria-hidden="true">
      <div className="format-paper">
        <div className="format-lines"><i /><i /><i /><i /><i /></div>
      </div>
      <span className={cn('format-badge', formatTone(value))}>{label}</span>
    </div>
  );
}

function LogoMark() {
  return (
    <img className="logo-mark" src="/ayst-arc-mark.png" alt="" />
  );
}

export default function Home() {
  const [kind, setKind] = useState<ConversionKind>('pdf_to_docx');
  const [files, setFiles] = useState<ConversionFile[]>([]);
  const [dragging, setDragging] = useState(false);
  const [options, setOptions] = useState<ConversionOptions>({ imageFormat: 'png', dpi: 200, pages: 'All', mergeImages: false });
  const [outputDir, setOutputDir] = useState('');
  const [notice, setNotice] = useState('');
  const [activeJobId, setActiveJobId] = useState<string>();
  const [cancelling, setCancelling] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [selfCheckStatus, setSelfCheckStatus] = useState<'checking' | 'ready' | 'repairable'>('checking');
  const [selfCheck, setSelfCheck] = useState<SelfCheck>();
  const [showChecks, setShowChecks] = useState(false);
  const [desktop, setDesktop] = useState(false);
  const [diagnosticCopied, setDiagnosticCopied] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);
  const activeJobIdRef = useRef<string | undefined>(undefined);
  const startingRef = useRef(false);
  const lastSeqRef = useRef(0);
  const engineAvailableRef = useRef(true);
  const submittedIdsRef = useRef<string[]>([]);
  const submittedMergeRef = useRef(false);
  const pendingProgressRef = useRef<DesktopProgress[]>([]);
  const mode = useMemo(() => conversionModes.find((item) => item.kind === kind) ?? conversionModes[0], [kind]);
  const groupModes = useMemo(() => {
    const seenOutputs = new Set<string>();
    return conversionModes.filter((item) => {
      if (item.group !== mode.group || seenOutputs.has(item.output)) return false;
      seenOutputs.add(item.output);
      return true;
    });
  }, [mode.group]);
  const hasActiveWork = submitting || Boolean(activeJobId);

  useEffect(() => setDesktop(isDesktopRuntime()), []);

  useEffect(() => {
    if (!desktop) {
      setSelfCheckStatus('ready');
      return;
    }
    let active = true;
    void getSelfCheck().then((result) => {
      if (!active || !engineAvailableRef.current) return;
      setSelfCheck(result);
      setSelfCheckStatus(result.status);
      if (result.status !== 'ready') setNotice('The local engine self-check needs attention.');
    }).catch(() => {
      if (!active || !engineAvailableRef.current) return;
      setSelfCheckStatus('repairable');
      setNotice('The local engine self-check could not complete.');
    });
    return () => {
      active = false;
    };
  }, [desktop]);

  useEffect(() => {
    if (!desktop) return;
    return listenToEngineTermination(() => {
      engineAvailableRef.current = false;
      const submittedIds = submittedIdsRef.current;
      setFiles((current) => projectEngineTermination(current, submittedIds));
      startingRef.current = false;
      activeJobIdRef.current = undefined;
      submittedIdsRef.current = [];
      submittedMergeRef.current = false;
      pendingProgressRef.current = [];
      setActiveJobId(undefined);
      setSubmitting(false);
      setCancelling(false);
      setSelfCheckStatus('repairable');
      setSelfCheck((current) => current ? {
        ...current,
        status: 'repairable',
        checks: [...current.checks.filter((check) => check.id !== 'engine_runtime'), {
          id: 'engine_runtime', status: 'failed', message: 'The local engine stopped unexpectedly. Restart the app.'
        }]
      } : undefined);
      setNotice('The local conversion engine stopped unexpectedly. Restart the app.');
    });
  }, [desktop]);

  async function copyDiagnostics() {
    if (!desktop) return;
    try {
      const text = await getDiagnosticInfo();
      await copyText(text);
      setDiagnosticCopied(true);
      setNotice('Diagnostics copied. File names and contents are not included.');
      window.setTimeout(() => setDiagnosticCopied(false), 2200);
    } catch {
      setNotice('Could not copy diagnostics. Try again.');
    }
  }

  function handleProgress(event: DesktopProgress) {
    const currentJobId = activeJobIdRef.current;
    if (!currentJobId) {
      if (startingRef.current) pendingProgressRef.current.push(event);
      return;
    }
    if (event.job_id !== currentJobId || event.seq <= lastSeqRef.current) return;
    lastSeqRef.current = event.seq;
    const submittedIds = submittedIdsRef.current;
    const submittedMerge = submittedMergeRef.current;
    setFiles((current) => projectJobProgress(current, submittedIds, event, submittedMerge));
    if (jobHasFinished(event)) {
      startingRef.current = false;
      activeJobIdRef.current = undefined;
      submittedIdsRef.current = [];
      submittedMergeRef.current = false;
      pendingProgressRef.current = [];
      setActiveJobId(undefined);
      setSubmitting(false);
      setCancelling(false);
      setNotice(event.message);
    }
  }

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let active = true;
    void listenToProgress((event) => {
      if (!active) return;
      handleProgress(event);
    }).then((cleanup) => {
      if (active) unlisten = cleanup;
      else cleanup();
    });
    return () => {
      active = false;
      unlisten?.();
    };
  }, [desktop]);

  useEffect(() => {
    if (!desktop) return;
    const preventNavigation = (event: globalThis.DragEvent) => event.preventDefault();
    window.addEventListener('dragover', preventNavigation);
    window.addEventListener('drop', preventNavigation);
    return () => {
      window.removeEventListener('dragover', preventNavigation);
      window.removeEventListener('drop', preventNavigation);
    };
  }, [desktop]);

  function addFiles(incoming: File[]) {
    const supported = incoming.filter((file) => accepts(kind, file));
    const rejected = incoming.length - supported.length;
    const next = supported.map((file) => ({
      id: `${file.name}-${file.lastModified}-${Math.random().toString(16).slice(2)}`,
      file,
      size: file.size,
      path: (file as File & { path?: string }).path,
      state: 'queued' as const,
      progress: 0,
      outputName: outputName(kind, file.name, options)
    }));
    setFiles((current) => [...current, ...next]);
    setNotice(rejectedFileNotice(kind, rejected));
  }

  async function addNativeFiles(paths: string[]) {
    const uniquePaths = [...new Set(paths)].filter(isAbsolutePath);
    if (!uniquePaths.length) return;
    try {
      const inspected = await inspectInputFiles(uniquePaths);
      const accepted = inspected.filter((item) => acceptsName(kind, item.name));
      const rejected = inspected.length - accepted.length;
      const next = accepted.map((item) => ({
        id: `${item.path}-${Math.random().toString(16).slice(2)}`,
        file: new File([], item.name),
        size: item.size,
        path: item.path,
        state: 'queued' as const,
        progress: 0,
        outputName: outputName(kind, item.name, options)
      }));
      setFiles((current) => {
        const existing = new Set(current.map((file) => file.path));
        return [...current, ...next.filter((file) => !existing.has(file.path))];
      });
      setNotice(rejectedFileNotice(kind, rejected));
    } catch {
      setNotice('Could not read the dropped files. Choose regular local files.');
    }
  }

  function onFileChange(event: ChangeEvent<HTMLInputElement>) {
    const selected = Array.from(event.target.files ?? []);
    if (desktop) void addNativeFiles(selected.map(pathForFile));
    else addFiles(selected);
    event.target.value = '';
  }

  function onDrop(event: DragEvent<HTMLDivElement>) {
    event.preventDefault();
    setDragging(false);
    const dropped = Array.from(event.dataTransfer.files);
    if (desktop) {
      const paths = dropped.map(pathForFile).filter(Boolean);
      if (paths.length) void addNativeFiles(paths);
      else setNotice('Could not resolve the dropped file path. Use “Choose files”.');
    } else addFiles(dropped);
  }

  function removeFile(id: string) {
    if (hasActiveWork) return;
    setFiles((current) => current.filter((item) => item.id !== id));
  }

  async function submitJob(selectedFiles: ConversionFile[]) {
    if (startingRef.current || activeJobIdRef.current) return;
    if (!desktop) {
      setNotice('Preview mode can only add files. Run the desktop app to convert locally.');
      return;
    }
    setNotice('');
    const selectedIds = new Set(selectedFiles.map((file) => file.id));
    setFiles((current) => current.map((file) => selectedIds.has(file.id) ? { ...file, state: 'running', progress: 8, message: 'Submitting local task', outputs: undefined } : file));
    const inputs = selectedFiles.map((item) => item.path).filter((path): path is string => Boolean(path));
    if (inputs.length !== selectedFiles.length || inputs.some((path) => !isAbsolutePath(path))) {
      setFiles((current) => current.map((file) => selectedIds.has(file.id) ? { ...file, state: 'queued', progress: 0 } : file));
      setNotice('The desktop runtime could not resolve the file path. Drag it from Finder or Explorer.');
      return;
    }
    const normalizedOutputDir = outputDir.trim() || undefined;
    if (normalizedOutputDir && !isAbsolutePath(normalizedOutputDir)) {
      setFiles((current) => current.map((file) => selectedIds.has(file.id) ? { ...file, state: 'queued', progress: 0 } : file));
      setNotice('The output directory must be an absolute path. Use the folder picker.');
      return;
    }
    activeJobIdRef.current = undefined;
    setActiveJobId(undefined);
    lastSeqRef.current = 0;
    submittedIdsRef.current = selectedFiles.map((file) => file.id);
    submittedMergeRef.current = kind === 'image_to_pdf' && options.mergeImages === true;
    pendingProgressRef.current = [];
    startingRef.current = true;
    setSubmitting(true);
    setCancelling(false);
    try {
      const jobOptions = kind === 'pdf_to_image' || kind === 'pdf_to_txt'
        ? options
        : { ...options, pages: 'All', dpi: undefined, jpgQuality: undefined };
      const jobId = await startJob({ kind, inputs, outputDir: normalizedOutputDir, options: jobOptions });
      activeJobIdRef.current = jobId;
      startingRef.current = false;
      setActiveJobId(jobId);
      setNotice('Local task started. Waiting for conversion events.');
      const pending = pendingProgressRef.current;
      pendingProgressRef.current = [];
      for (const event of pending) handleProgress(event);
    } catch (error) {
      startingRef.current = false;
      activeJobIdRef.current = undefined;
      submittedIdsRef.current = [];
      submittedMergeRef.current = false;
      pendingProgressRef.current = [];
      setActiveJobId(undefined);
      setSubmitting(false);
      const errorMessage = error instanceof Error ? error.message : String(error);
      const message = engineAvailableRef.current ? startJobErrorMessage(error, kind) : 'The local conversion engine stopped unexpectedly. Restart the app.';
      setFiles((current) => current.map((file) => selectedIds.has(file.id) ? { ...file, state: 'failed', progress: 0, message } : file));
      setNotice(message);
    }
  }

  async function beginConversion() {
    if (hasActiveWork) return;
    const pending = files.filter((file) => file.state === 'queued');
    if (!pending.length) {
      setNotice('There are no queued files.');
      return;
    }
    await submitJob(pending);
  }

  async function retryFailed() {
    if (hasActiveWork) return;
    const retryable = files.filter((file) => file.state === 'failed' || file.state === 'cancelled' || file.state === 'timed_out');
    if (!retryable.length) return;
    await submitJob(retryable);
  }

  async function stopConversion() {
    if (!activeJobIdRef.current || cancelling) return;
    setCancelling(true);
    try {
      await cancelJob(activeJobIdRef.current);
      setNotice('Cancelling local task…');
    } catch {
      setCancelling(false);
      setNotice('The cancel request could not be submitted.');
    }
  }

  async function chooseFiles() {
    if (!desktop) {
      fileInput.current?.click();
      return;
    }
    try {
      const paths = await chooseInputFiles();
      await addNativeFiles(paths);
    } catch {
      setNotice('Could not open the file picker.');
    }
  }

  async function chooseOutput() {
    if (!desktop) {
      setNotice('Preview mode cannot choose a desktop output directory.');
      return;
    }
    try {
      const selected = await chooseOutputDir();
      if (selected) setOutputDir(selected);
    } catch {
      setNotice('Could not choose the output directory.');
    }
  }

  async function openOutput(path: string) {
    try {
      await openPath(path);
    } catch {
      setNotice('Could not open the output file.');
    }
  }

  async function openOutputDirectory(path: string) {
    const directory = parentPath(path);
    if (!directory) {
      setNotice('Could not determine the output directory.');
      return;
    }
    try {
      await openPath(directory);
    } catch {
      setNotice('Could not open the output directory.');
    }
  }

  function parentPath(path: string) {
    const normalized = path.replace(/[\\/]$/, '');
    const separator = Math.max(normalized.lastIndexOf('/'), normalized.lastIndexOf('\\'));
    return separator > 0 ? normalized.slice(0, separator) : '';
  }

  function isAbsolutePath(value: string) {
    return value.startsWith('/') || value.startsWith('\\\\') || /^[A-Za-z]:[\\/]/.test(value);
  }

  function switchMode(nextKind: ConversionKind) {
    if (hasActiveWork) return;
    setKind(nextKind);
    if (nextKind !== 'image_to_pdf') setOptions((current) => ({ ...current, mergeImages: false }));
    activeJobIdRef.current = undefined;
    setActiveJobId(undefined);
    setFiles([]);
    setNotice('');
  }

  function switchGroup(nextGroup: ConversionGroup) {
    const first = conversionModes.find((item) => item.group === nextGroup);
    if (first) switchMode(first.kind);
  }

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand">
          <div className="brand-mark"><LogoMark /></div>
          <p className="brand-title">Ayst Arc PDF</p>
        </div>
        <div className="top-actions">
          <button className="icon-button" type="button" aria-label="Open installation check" title="Installation check" onClick={() => setShowChecks((current) => !current)}><Settings2 aria-hidden="true" size={15} /></button>
        </div>
      </header>

      <main className="main">
        <section className="intro">
          <h1 className="headline">PDF Converter</h1>
        </section>

        {desktop && showChecks && selfCheck && (
          <section className="check-panel" aria-label="Installation check results">
            <div className="check-heading"><div><p className="panel-kicker">Installation check</p><p className="check-summary">{selfCheck.status === 'ready' ? 'Local engine ready' : 'Action required'}</p></div><div className="check-actions"><button className="text-button" type="button" onClick={() => void copyDiagnostics()}><CopyIcon aria-hidden="true" size={14} /> {diagnosticCopied ? 'Copied' : 'Copy diagnostics'}</button><span className={cn('check-state', selfCheck.status)}>{selfCheck.status === 'ready' ? 'Ready' : 'Action required'}</span></div></div>
            <div className="check-list">{selfCheck.checks.map((check) => <div className="check-row" key={check.id}><span className={cn('check-icon', check.status)}>{check.status === 'passed' ? <Check aria-hidden="true" size={13} /> : <Info aria-hidden="true" size={13} />}</span><span>{check.message}</span></div>)}</div>
          </section>
        )}

        <section className="format-flow" aria-label="Conversion formats">
          <article className="format-card source-card">
            <div className="format-card-head"><span>Input</span></div>
            <div className="format-preview"><FormatIllustration value={mode.input} /><strong>{formatToken(mode.input)}</strong></div>
            <select id="source-format" aria-label="Input file type" className="select-control flow-select" value={mode.group} disabled={hasActiveWork} onChange={(event) => switchGroup(event.target.value as ConversionGroup)}>
              {conversionGroups.map((group) => <option key={group.id} value={group.id}>{group.title}</option>)}
            </select>
          </article>

          <div className="flow-arrow" aria-hidden="true"><ArrowRight size={20} /></div>

          <article className="format-card target-card">
            <div className="format-card-head"><span>Convert to</span></div>
            <div className="target-grid">
              {groupModes.map((item) => (
                <button key={item.kind} type="button" className={cn('target-option', item.kind === kind && 'active')} disabled={hasActiveWork} onClick={() => switchMode(item.kind)}>
                  <FormatIllustration value={item.output} compact />
                  <span className="target-title">{item.output}</span>
                </button>
              ))}
            </div>
          </article>
        </section>

        <section className="workspace">
          <div className="panel panel-main">
            <div className={cn('drop-zone', dragging && 'dragging')} onDragEnter={(event) => { event.preventDefault(); setDragging(true); }} onDragOver={(event) => event.preventDefault()} onDragLeave={() => setDragging(false)} onDrop={onDrop}>
              <div><div className="drop-icon"><Upload aria-hidden="true" size={18} /></div><p className="drop-title">Drop files here</p><button className="choose-button" type="button" onClick={chooseFiles}><MousePointer2 aria-hidden="true" size={14} /> Choose files</button><input ref={fileInput} className="hidden-input" type="file" multiple accept={inputAccept(kind)} onChange={onFileChange} /></div>
            </div>

            {files.length > 0 && <div className="file-list" aria-live="polite">{files.map((item) => <div className="file-row" key={item.id}><span className="file-type"><FileArchive aria-hidden="true" size={16} /></span><div><p className="file-name" title={item.file.name}>{item.file.name}</p><p className="file-meta">{formatSize(item.size)}</p>{(item.state === 'failed' || item.state === 'cancelled' || item.state === 'timed_out') && item.message && <p className="file-message">{item.message}</p>}</div><div className={cn('file-status', item.state)}>{item.state === 'running' ? <LoaderCircle aria-hidden="true" size={13} className="spin" /> : item.state === 'succeeded' ? <Check aria-hidden="true" size={13} /> : item.state === 'failed' || item.state === 'cancelled' || item.state === 'timed_out' ? <Info aria-hidden="true" size={13} /> : <span>{fileStatusLabel(item)}</span>}{item.state === 'running' ? `${item.progress}%` : item.state === 'succeeded' ? 'Done' : item.state === 'failed' ? 'Action required' : item.state === 'cancelled' ? 'Cancelled' : item.state === 'timed_out' ? 'Timed out' : null}{item.state === 'succeeded' && item.outputs?.[0] && <button className="result-button" type="button" aria-label={`Open output for ${item.file.name}`} title="Open output" onClick={() => openOutput(item.outputs?.[0] ?? '')}><ExternalLink aria-hidden="true" size={14} /></button>}{item.state === 'succeeded' && item.outputs?.[0] && <button className="result-button" type="button" aria-label={`Open output folder for ${item.file.name}`} title="Open output folder" onClick={() => openOutputDirectory(item.outputs?.[0] ?? '')}><FolderOpen aria-hidden="true" size={14} /></button>}{item.state === 'failed' || item.state === 'cancelled' || item.state === 'timed_out' ? <button className="result-button" type="button" aria-label={`Retry ${item.file.name}`} title="Retry file" onClick={() => void submitJob([item])} disabled={hasActiveWork}><RotateCcw aria-hidden="true" size={14} /></button> : null}<button className="remove-button" type="button" aria-label={`Remove ${item.file.name}`} title="Remove file" onClick={() => removeFile(item.id)} disabled={hasActiveWork}><Trash2 aria-hidden="true" size={14} /></button></div></div>)}</div>}
            {notice && <div className="helper"><Info aria-hidden="true" size={16} /><span>{notice}</span></div>}
          </div>

          <aside className="panel panel-side">
            {kind === 'pdf_to_image' && <div className="option-group compact-option"><label className="section-label" htmlFor="format">Image format</label><select id="format" className="select-control" value={options.imageFormat} onChange={(event) => setOptions((current) => ({ ...current, imageFormat: event.target.value as 'png' | 'jpg' | 'bmp' | 'gif' | 'webp' | 'tiff' }))}><option value="png">PNG · Lossless</option><option value="jpg">JPG · Smaller</option><option value="webp">WebP · Compressed</option><option value="gif">GIF · 256 colors</option><option value="bmp">BMP · Raw bitmap</option><option value="tiff">TIFF · Lossless</option></select></div>}
            {kind === 'pdf_to_image' && <div className="option-group"><label className="section-label" htmlFor="dpi">Resolution</label><select id="dpi" className="select-control" value={options.dpi} onChange={(event) => setOptions((current) => ({ ...current, dpi: Number(event.target.value) as 150 | 200 | 300 }))}><option value="150">150 DPI · Fast</option><option value="200">200 DPI · Balanced</option><option value="300">300 DPI · Sharp</option></select></div>}
            {(kind === 'pdf_to_image' || kind === 'pdf_to_txt') && <div className="option-group"><label className="section-label" htmlFor="pages">Pages</label><input id="pages" className="text-control" value={options.pages} onChange={(event) => setOptions((current) => ({ ...current, pages: event.target.value }))} placeholder="All, or 1-3,8" /></div>}
            {kind === 'image_to_pdf' && <div className="option-group"><label className="merge-option"><input type="checkbox" checked={options.mergeImages === true} disabled={hasActiveWork} onChange={(event) => setOptions((current) => ({ ...current, mergeImages: event.target.checked }))} /> <span>Merge into one PDF</span></label></div>}
            <div className="option-group"><label className="section-label" htmlFor="output">Output folder</label><div className="output-row"><input id="output" className="text-control" value={outputDir} placeholder="Same folder as input" onChange={(event) => setOutputDir(event.target.value)} /><button className="icon-button" type="button" aria-label="Choose output folder" title="Choose output folder" onClick={chooseOutput}><FolderOpen aria-hidden="true" size={16} /></button></div></div>
            <div className="side-actions">{hasActiveWork ? <button className="outline-button" type="button" disabled={cancelling || !activeJobId} onClick={stopConversion}>{cancelling ? <LoaderCircle aria-hidden="true" size={15} className="spin" /> : <X aria-hidden="true" size={15} />} {cancelling ? 'Cancelling' : 'Cancel task'}</button> : <button className="primary-button" type="button" disabled={!files.some((file) => file.state === 'queued')} onClick={beginConversion}><ArrowRight aria-hidden="true" size={15} /> Convert</button>}{!hasActiveWork && files.some((file) => file.state === 'failed' || file.state === 'cancelled' || file.state === 'timed_out') && <button className="outline-button" type="button" onClick={retryFailed}><RotateCcw aria-hidden="true" size={15} /> Retry failed</button>}{files.length > 0 && <button className="outline-button" type="button" disabled={hasActiveWork} onClick={() => setFiles([])}><Minus aria-hidden="true" size={15} /> Clear files</button>}</div>
          </aside>
        </section>
      </main>
    </div>
  );
}
