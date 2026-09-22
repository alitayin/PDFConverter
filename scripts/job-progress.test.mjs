import assert from 'node:assert/strict';
import { test } from 'node:test';
import { jobHasFinished, projectEngineTermination, projectJobProgress } from '../src/lib/job-progress.ts';

function file(id, state = 'queued') {
  return { id, file: { name: `${id}.pdf` }, size: 100, state, progress: 0, outputs: state === 'succeeded' ? [`${id}.txt`] : undefined };
}

function event(state, completed, total, phase = 'done') {
  return {
    job_id: 'test-job', seq: 1, state, completed, total, phase,
    message: `event: ${state}`, outputs: state === 'succeeded' ? ['new.txt'] : undefined
  };
}

test('queued event only resets submitted files, preserving old failures and new arrivals', () => {
  const files = [file('done', 'succeeded'), file('retry', 'failed'), file('old-error', 'failed'), file('new')];
  const result = projectJobProgress(files, ['retry'], event('queued', 0, 1));
  assert.equal(result[1].state, 'queued');
  assert.equal(result[0], files[0]);
  assert.equal(result[2], files[2]);
  assert.equal(result[3], files[3]);
});

test('retry success maps completed index into submitted IDs, not the full list', () => {
  const files = [file('done', 'succeeded'), file('retry', 'running'), file('new')];
  const running = projectJobProgress(files, ['retry'], event('running', 0, 1, 'extracting'));
  assert.equal(running[1].progress, 58);
  assert.equal(running[0], files[0]);
  const success = projectJobProgress(running, ['retry'], event('succeeded', 1, 1));
  assert.equal(success[1].state, 'succeeded');
  assert.deepEqual(success[1].outputs, ['new.txt']);
  assert.equal(success[0], files[0]);
  assert.equal(success[2], files[2]);
  assert.equal(jobHasFinished(event('succeeded', 1, 1)), true);
});

test('single-file retry leaves other failed entries untouched', () => {
  const files = [file('failed-a', 'failed'), file('failed-b', 'running')];
  const result = projectJobProgress(files, ['failed-b'], event('succeeded', 1, 1));
  assert.equal(result[0], files[0]);
  assert.equal(result[1].state, 'succeeded');
});

test('batch failure then success maps each result to the correct submitted entry', () => {
  const files = [file('old', 'succeeded'), file('first', 'running'), file('second', 'running')];
  const failed = projectJobProgress(files, ['first', 'second'], event('failed', 1, 2, 'failed'));
  assert.equal(failed[1].state, 'failed');
  assert.equal(failed[2], files[2]);
  assert.equal(jobHasFinished(event('failed', 1, 2)), false);
  const success = projectJobProgress(failed, ['first', 'second'], event('succeeded', 2, 2));
  assert.equal(success[1].state, 'failed');
  assert.equal(success[2].state, 'succeeded');
  assert.equal(jobHasFinished(event('succeeded', 2, 2)), true);
});

test('cancelled batch only stops unfinished submitted files', () => {
  const files = [file('old-failed', 'failed'), file('done', 'succeeded'), file('active', 'running'), file('waiting'), file('new')];
  const result = projectJobProgress(files, ['done', 'active', 'waiting'], event('cancelled', 1, 3, 'cancelled'));
  assert.equal(result[0], files[0]);
  assert.equal(result[1], files[1]);
  assert.equal(result[2].state, 'cancelled');
  assert.equal(result[3].state, 'cancelled');
  assert.equal(result[4], files[4]);
  assert.equal(jobHasFinished(event('cancelled', 1, 3)), true);
});

test('timeout marks only this job while earlier and later files keep their states', () => {
  const files = [file('previous', 'failed'), file('active', 'running'), file('new')];
  const result = projectJobProgress(files, ['active'], event('timed_out', 0, 1, 'timed_out'));
  assert.equal(result[0], files[0]);
  assert.equal(result[1].state, 'timed_out');
  assert.equal(result[2], files[2]);
  assert.equal(jobHasFinished(event('timed_out', 0, 1)), true);
});

test('only final success or failure closes a naturally completed batch', () => {
  assert.equal(jobHasFinished(event('queued', 0, 1)), false);
  assert.equal(jobHasFinished(event('running', 0, 1, 'writing')), false);
  assert.equal(jobHasFinished(event('succeeded', 1, 2)), false);
  assert.equal(jobHasFinished(event('failed', 2, 2, 'failed')), true);
});

test('engine termination fails unfinished submitted files without changing completed files', () => {
  const files = [file('done', 'succeeded'), file('active', 'running'), file('waiting'), file('other')];
  const result = projectEngineTermination(files, ['done', 'active', 'waiting']);
  assert.equal(result[0], files[0]);
  assert.equal(result[1].state, 'failed');
  assert.equal(result[1].message, 'The local conversion engine stopped unexpectedly');
  assert.equal(result[2].state, 'failed');
  assert.equal(result[3], files[3]);
});

test('merged image batch updates every selected file with one shared result', () => {
  const files = [file('older', 'succeeded'), file('image-a'), file('image-b'), file('image-c')];
  const ids = ['image-a', 'image-b', 'image-c'];
  const running = projectJobProgress(files, ids, event('running', 2, 3, 'batch'), true);
  assert.equal(running[0], files[0]);
  assert.deepEqual(running.slice(1).map((item) => item.state), ['running', 'running', 'running']);
  assert.ok(running[1].progress > 8 && running[1].progress < 100);
  const succeeded = projectJobProgress(running, ids, event('succeeded', 3, 3, 'batch_done'), true);
  assert.deepEqual(succeeded.slice(1).map((item) => item.state), ['succeeded', 'succeeded', 'succeeded']);
  assert.deepEqual(succeeded.slice(1).map((item) => item.outputs), [['new.txt'], ['new.txt'], ['new.txt']]);
  assert.equal(jobHasFinished(event('succeeded', 3, 3, 'batch_done')), true);
});

test('merged image failure or cancellation does not leave selected files running', () => {
  const files = [file('image-a', 'running'), file('image-b', 'running'), file('other')];
  for (const state of ['failed', 'cancelled', 'timed_out']) {
    const result = projectJobProgress(files, ['image-a', 'image-b'], event(state, state === 'failed' ? 2 : 0, 2), true);
    assert.deepEqual(result.slice(0, 2).map((item) => item.state), [state, state]);
    assert.equal(result[2], files[2]);
    assert.equal(jobHasFinished(event(state, state === 'failed' ? 2 : 0, 2)), true);
  }
});
