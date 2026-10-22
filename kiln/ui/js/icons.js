// The design's 8×8 pixel icons, drawn on the grid.

import { svg } from './dom.js';

const PATHS = {
  overview: 'M3 0h2v1H3zM2 1h4v1H2zM1 2h6v1H1zM0 3h8v1H0zM1 4h2v4H1zM5 4h2v4H5z',
  instances: 'M0 0h8v2H0zM0 2h1v5H0zM7 2h1v5H7zM2 3h1v1H2zM3 4h1v1H3zM2 5h1v1H2zM4 5h2v1H4zM0 7h8v1H0z',
  images: 'M2 0h4v1H2zM1 1h1v1H1zM6 1h1v1H6zM0 2h1v4H0zM7 2h1v4H7zM3 3h2v2H3zM1 6h1v1H1zM6 6h1v1H6zM2 7h4v1H2z',
  nodes: 'M0 0h8v1H0zM0 1h6v1H0zM7 1h1v1H7zM0 3h8v1H0zM0 4h6v1H0zM7 4h1v1H7zM0 6h8v1H0zM0 7h6v1H0zM7 7h1v1H7z',
  quotas: 'M0 0h3v2H0zM0 3h5v2H0zM0 6h8v2H0z',
  people: 'M3 0h2v1H3zM2 1h1v2H2zM5 1h1v2H5zM3 3h2v1H3zM1 5h6v1H1zM0 6h1v2H0zM7 6h1v2H7z',
  lock: 'M2 0h4v1H2zM1 1h1v2H1zM6 1h1v2H6zM0 3h8v5H0z',
};

export function icon(name, fill = 'currentColor') {
  return svg('svg', { 'aria-hidden': 'true', width: 16, height: 16, viewBox: '0 0 8 8', 'shape-rendering': 'crispEdges', fill, class: 'icon' },
    svg('path', { d: PATHS[name] }));
}
