/* Loads the repository's Rust core compiled to WebAssembly.
   Each run stays in the browser and returns bounded frame and controller data. */
import init, { run_demo } from './pkg/fieldbus_demo_wasm.js';

const elements = {
  scenario: document.querySelector('#scenario'),
  cycles: document.querySelector('#cycles'),
  fault: document.querySelector('#fault-cycle'),
  run: document.querySelector('#run'),
  status: document.querySelector('#run-status'),
  chart: document.querySelector('#response-chart')
};
const wasmReady = init();

function syncInputs() {
  const maxFault = Math.max(10, Number(elements.cycles.value) - 10);
  elements.fault.max = String(maxFault);
  if (Number(elements.fault.value) > maxFault) elements.fault.value = String(maxFault);
  document.querySelector('#cycles-value').textContent = elements.cycles.value;
  document.querySelector('#fault-value').textContent = elements.fault.value;
}

function addSvg(svg, tag, attrs, text = '') {
  const node = document.createElementNS('http://www.w3.org/2000/svg', tag);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  if (text) node.textContent = text;
  svg.append(node);
  return node;
}

function drawResponse(result) {
  const svg = elements.chart;
  svg.replaceChildren();
  const points = result.samples.filter((_, index) => index % Math.max(1, Math.ceil(result.samples.length / 500)) === 0);
  const width = 700;
  const height = 222;
  const left = 46;
  const top = 20;
  const x = cycle => left + (cycle / Math.max(1, result.cycles - 1)) * width;
  const y = value => top + ((1000 - value) / 2000) * height;
  for (const value of [-1000, -500, 0, 500, 1000]) {
    const py = y(value);
    addSvg(svg, 'line', { x1: left, y1: py, x2: left + width, y2: py, class: value === 0 ? 'zero-line' : 'grid-line' });
    addSvg(svg, 'text', { x: left - 8, y: py + 4, 'text-anchor': 'end', class: 'axis-label' }, String(value));
  }
  addSvg(svg, 'line', { x1: left, y1: top + height, x2: left + width, y2: top + height, class: 'axis-line' });
  addSvg(svg, 'text', { x: left, y: 278, class: 'axis-label' }, 'cycle 0');
  addSvg(svg, 'text', { x: left + width, y: 278, 'text-anchor': 'end', class: 'axis-label' }, `cycle ${result.cycles - 1}`);
  if (result.summary.safe_cycle !== null) {
    const tripX = x(result.summary.safe_cycle);
    addSvg(svg, 'rect', { x: tripX, y: top, width: Math.max(1, left + width - tripX), height, class: 'safe-zone' });
    addSvg(svg, 'line', { x1: tripX, y1: top, x2: tripX, y2: top + height, class: 'trip-line' });
    addSvg(svg, 'text', { x: Math.min(tripX + 7, left + width - 90), y: top + 15, class: 'trip-label' }, 'SAFE STATE LATCHED');
  }
  const path = points.map((point, index) => `${index ? 'L' : 'M'}${x(point.cycle).toFixed(2)},${y(point.actuator).toFixed(2)}`).join(' ');
  addSvg(svg, 'path', { d: path, class: 'actuator-line' });
  svg.setAttribute('aria-label', `Actuator output for ${result.mode} scenario. Accepted ${result.summary.accepted} frames, rejected ${result.summary.rejected}. Safe state ${result.summary.safe_latched ? 'latched' : 'not latched'}.`);
}

function showResult(result) {
  document.querySelector('#accepted').textContent = result.summary.accepted.toLocaleString();
  document.querySelector('#rejected').textContent = result.summary.rejected.toLocaleString();
  document.querySelector('#safe-state').textContent = result.summary.safe_latched ? 'Latched' : 'Not tripped';
  document.querySelector('#reaction').textContent = result.summary.safe_latched ? `${(result.summary.reaction_us / 1000).toFixed(0)} ms` : 'No trip';
  const badge = document.querySelector('#result-badge');
  badge.textContent = result.summary.safe_latched ? 'Safe state latched' : 'Run complete';
  badge.className = `badge ${result.summary.safe_latched ? 'badge-alert' : 'badge-good'}`;
  document.querySelector('#result-detail').textContent = result.mode === 'bad_crc'
    ? `The valid sensor frame was accepted. The extra corrupted copy was rejected by the CRC check. The watchdog remained fed by the accepted frame.`
    : result.summary.safe_latched
      ? `No sensor frame was accepted after cycle ${result.fault_cycle}. The 80 ms timeout was exceeded and the core latched safe at cycle ${result.summary.safe_cycle}.`
      : `Every sensor frame passed validation. The controller stayed active for ${result.cycles} cycles.`;
  drawResponse(result);
}

async function runScenario() {
  elements.run.disabled = true;
  elements.status.textContent = 'Running the Rust core in WebAssembly...';
  try {
    await wasmReady;
    const mode = elements.scenario.value;
    const cycles = Math.min(1000, Math.max(100, Number(elements.cycles.value)));
    const faultCycle = Math.min(cycles - 10, Math.max(10, Number(elements.fault.value)));
    const result = JSON.parse(run_demo(mode, cycles, faultCycle));
    showResult(result);
    elements.status.textContent = `Completed ${result.cycles} cycles in your browser.`;
  } catch (error) {
    elements.status.textContent = `The simulator could not start: ${error.message}`;
  } finally {
    elements.run.disabled = false;
  }
}

elements.cycles.addEventListener('input', syncInputs);
elements.fault.addEventListener('input', syncInputs);
elements.run.addEventListener('click', runScenario);
syncInputs();
document.querySelector('#provenance').textContent = 'Rust core / WebAssembly';
runScenario();
