import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { test } from 'node:test';
import { conversionModes, inputAccept } from '../src/lib/conversion.ts';

const require = createRequire(import.meta.url);
const { inputExtensions } = require('./input-extensions.cjs');

test('native file picker includes every working-tree conversion input', () => {
  const expected = new Set(conversionModes.flatMap(({ kind }) =>
    inputAccept(kind).split(',').map((extension) => extension.slice(1))));
  assert.deepEqual([...inputExtensions].sort(), [...expected].sort());
});
