#!/usr/bin/env node
// Portable one-shot SQLite writer for the independent probes.
//
// Prefers Node's built-in `node:sqlite` (Node >=22), then the `sqlite3` CLI.
// Returns the driver used, or null when the host has neither. Probes treat a
// null driver as a documented skip (the security property itself is also
// covered by the Rust unit suites), never as a pass.
import { execFileSync } from 'node:child_process';

export async function sqliteExec(path, sql) {
  try {
    const { DatabaseSync } = await import('node:sqlite');
    const db = new DatabaseSync(path);
    try {
      db.exec(sql);
    } finally {
      db.close();
    }
    return 'node:sqlite';
  } catch {
    try {
      execFileSync('sqlite3', [path, sql], { stdio: 'pipe' });
      return 'sqlite3';
    } catch {
      return null;
    }
  }
}
