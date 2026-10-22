// Who is signed in, and which project the console is looking at.

import { get, post } from './api.js';

export const session = {
  me: null,
  /** [{ name, owner }] as kiln lists them. */
  projects: [],
  /** Path of the current project: `name`, or `owner:name` for someone else's. */
  project: null,
};

const KEY = 'copper.project';

export const isAdmin = () => session.me?.role === 'admin';

export function projectPath(p) {
  return p.owner === session.me.name ? p.name : `${p.owner}:${p.name}`;
}

/** The current project as `{ owner, name }`. */
export function currentProject() {
  const [owner, name] = session.project.includes(':') ? session.project.split(':') : [session.me.name, session.project];
  return { owner, name };
}

export async function loadSession() {
  session.me = await get('/v1/me');
  await loadProjects();
}

export async function loadProjects() {
  session.projects = await get('/v1/projects');
  const paths = session.projects.map(projectPath);
  let saved = null;
  try { saved = localStorage.getItem(KEY); } catch { /* storage may be off */ }
  session.project = paths.includes(saved) ? saved : (paths.find((p) => !p.includes(':')) ?? paths[0] ?? null);
}

export function setProject(path) {
  session.project = path;
  try { localStorage.setItem(KEY, path); } catch { /* fine */ }
}

export async function createProject(name) {
  await post('/v1/projects', { name });
  await loadProjects();
  setProject(name);
}

export function forget() {
  session.me = null;
  session.projects = [];
  session.project = null;
}
