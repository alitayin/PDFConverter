'use strict';

const { lstatSync } = require('node:fs');
const { isAbsolute, join } = require('node:path');

const OFFICE_EXECUTABLE_ENV = 'MINIMALPDF_OFFICE_EXECUTABLE';
const OFFICE_MODE_ENV = 'MINIMALPDF_OFFICE_MODE';
const OFFICE_PATHS = {
  darwin: ['office', 'LibreOffice.app', 'Contents', 'MacOS', 'soffice'],
  win32: ['office', 'LibreOffice', 'program', 'soffice.exe'],
};

function bundledOfficeExecutable({ isPackaged, resourcesPath, platform }) {
  const parts = Object.hasOwn(OFFICE_PATHS, platform) ? OFFICE_PATHS[platform] : null;
  if (!isPackaged || !parts || typeof resourcesPath !== 'string' || !isAbsolute(resourcesPath)) return null;

  let current = resourcesPath;
  try {
    const root = lstatSync(current);
    if (!root.isDirectory() || root.isSymbolicLink()) return null;
  } catch {
    return null;
  }
  for (const [index, part] of parts.entries()) {
    current = join(current, part);
    let entry;
    try {
      entry = lstatSync(current);
    } catch {
      return null;
    }
    if (entry.isSymbolicLink()) return null;
    if (index === parts.length - 1) {
      if (!entry.isFile() || entry.size === 0) return null;
    } else if (!entry.isDirectory()) {
      return null;
    }
  }
  return current;
}

function officeBridgeEnv({ baseEnv = process.env, isPackaged, resourcesPath, platform }) {
  const env = { ...baseEnv };
  delete env[OFFICE_EXECUTABLE_ENV];
  delete env[OFFICE_MODE_ENV];
  if (isPackaged) {
    env[OFFICE_MODE_ENV] = 'bundled';
    env.PYTHONDONTWRITEBYTECODE = '1';
  }
  const executable = bundledOfficeExecutable({ isPackaged, resourcesPath, platform });
  if (executable) env[OFFICE_EXECUTABLE_ENV] = executable;
  return env;
}

module.exports = { bundledOfficeExecutable, officeBridgeEnv };
