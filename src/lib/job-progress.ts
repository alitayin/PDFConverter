import type { ConversionFile } from './conversion';
import type { DesktopProgress } from './desktop';

export function jobHasFinished(event: DesktopProgress) {
  return event.state === 'cancelled' || event.state === 'timed_out'
    || ((event.state === 'succeeded' || event.state === 'failed') && event.completed >= event.total);
}

export function projectJobProgress(files: ConversionFile[], submittedIds: readonly string[], event: DesktopProgress, mergeImages = false) {
  const selected = new Set(submittedIds);
  const currentId = submittedIds[event.completed];
  const finishedId = submittedIds[event.completed - 1];
  const remaining = new Set(submittedIds.slice(event.completed));

  return files.map((file) => {
    if (!selected.has(file.id)) return file;
    if (event.state === 'queued') {
      return { ...file, state: 'queued' as const, progress: 0, message: event.message };
    }
    if (mergeImages) {
      if (event.state === 'succeeded') return { ...file, state: 'succeeded' as const, progress: 100, message: event.message, outputs: event.outputs };
      if (event.state === 'failed' || event.state === 'cancelled' || event.state === 'timed_out') {
        return { ...file, state: event.state, progress: 0, message: event.message };
      }
      if (event.state === 'running') {
        const progress = event.total > 0 ? Math.min(95, Math.max(8, Math.round(event.completed / event.total * 90))) : 8;
        return { ...file, state: 'running' as const, progress, message: event.message };
      }
    }
    if (event.state === 'succeeded' && file.id === finishedId) {
      return { ...file, state: 'succeeded' as const, progress: 100, message: event.message, outputs: event.outputs };
    }
    if (event.state === 'failed' && file.id === finishedId) {
      return { ...file, state: 'failed' as const, progress: 0, message: event.message };
    }
    if ((event.state === 'cancelled' || event.state === 'timed_out') && remaining.has(file.id)) {
      return { ...file, state: event.state, progress: 0, message: event.message };
    }
    if (event.state === 'running' && file.id === currentId) {
      const progress = event.phase === 'reading' ? 12 : event.phase === 'extracting' ? 58 : event.phase === 'writing' ? 86 : 8;
      return { ...file, state: 'running' as const, progress, message: event.message };
    }
    return file;
  });
}

export function projectEngineTermination(files: ConversionFile[], submittedIds: readonly string[]) {
  const selected = new Set(submittedIds);
  return files.map((file) => selected.has(file.id) && (file.state === 'queued' || file.state === 'running')
    ? { ...file, state: 'failed' as const, progress: 0, message: 'The local conversion engine stopped unexpectedly' }
    : file);
}
