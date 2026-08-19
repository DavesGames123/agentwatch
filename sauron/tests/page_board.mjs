#!/usr/bin/env node
// Does the page render a real board without throwing?
//
// `web_workspace.mjs` proves the server end: sockets, ptys, keystrokes. This
// proves the other end, which no Rust test can reach -- a TypeError anywhere in
// `drawBoard` leaves a blank window and a server that looks perfectly healthy.
//
// The board it renders is a real message from a real `sauron serve`, not a
// fixture, so the assertions run against whatever this repo's sessions actually
// look like right now. What is stubbed is only the DOM and xterm.js; the script
// under test is the one that ships, executed as written.
//
// Run, from the sauron/ directory:
//   cargo build --release && node tests/page_board.mjs
//
// Not wired into `cargo test` -- see the note in web_workspace.mjs.

import { readFileSync } from "node:fs";
import { spawn } from "node:child_process";
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

const PORT = 7409;
const server = spawn("target/release/sauron", ["serve", "--port", String(PORT), "--agents", "0"], { stdio: "ignore" });
process.on("exit", () => server.kill("SIGKILL"));
await wait(2500);

// A real board message off a real server.
const probe = new WebSocket(`ws://127.0.0.1:${PORT}/ws`);
const boardMsg = await new Promise((res) => {
  probe.addEventListener("message", (e) => {
    if (typeof e.data === "string" && JSON.parse(e.data).t === "board") res(e.data);
  });
});
probe.close();
server.kill();

const html = readFileSync("assets/sauron_web.html", "utf8");
const js = html.match(/<script>\n"use strict";\n([\s\S]*)\n<\/script>/)[1];

// --- a DOM just big enough ------------------------------------------------
const made = [];
const node = (tag = "div") => {
  const n = {
    tagName: tag, className: "", id: "", textContent: "", hidden: false, title: "",
    // Custom properties are recorded rather than dropped: the servant colour
    // reaches the screen only as `--servant` and `--dot`, so a stub that
    // swallows them cannot tell a coloured board from a grey one.
    children: [], dataset: {},
    style: { props: {}, setProperty(k, v) { this.props[k] = v; } },
    classList: { toggle() {}, add() {}, remove() {}, contains: () => false },
    append(...c) { this.children.push(...c); },
    replaceChildren(...c) { this.children = c; },
    appendChild(c) { this.children.push(c); return c; },
    remove() {}, focus() {}, contains: () => false,
    addEventListener() {}, removeEventListener() {},
    querySelector: () => node(), querySelectorAll: () => [],
    getBoundingClientRect: () => ({ left: 0, top: 0, right: 100, bottom: 20, width: 100, height: 20 }),
    get offsetWidth() { return 200; },
    set onclick(f) { this._click = f; }, get onclick() { return this._click; },
  };
  made.push(n);
  return n;
};
const byId = {};
for (const id of ["board", "terms", "tabs", "bar", "eye", "repo", "path", "tallies", "menu", "toast", "conn"]) {
  byId[id] = node(); byId[id].id = id;
}
globalThis.document = {
  getElementById: (id) => byId[id] || node(),
  createElement: (t) => node(t),
  addEventListener() {},
  title: "",
};
globalThis.location = { host: "127.0.0.1:7409" };
globalThis.window = { innerWidth: 1440, innerHeight: 900, isSecureContext: true, addEventListener() {} };
globalThis.requestAnimationFrame = (cb) => cb();
Object.defineProperty(globalThis, "navigator", { value: { clipboard: {} }, configurable: true });
const spawned = [];
globalThis.Terminal = class {
  constructor(opts) { this.options = { ...opts }; spawned.push(this); }
  loadAddon() {} open(el) { this.host = el; } onData() {} write() {} focus() {} reset() {} dispose() {}
  get cols(){return 80} get rows(){return 24}
};
globalThis.FitAddon = { FitAddon: class { fit() {} } };
const sockets = [];
globalThis.WebSocket = class {
  constructor() { this.readyState = 1; sockets.push(this); this._l = {}; }
  addEventListener(n, f) { (this._l[n] ??= []).push(f); }
  set onopen(f) { this._l.open = [f]; } set onclose(f) { this._l.close = [f]; }
  set onmessage(f) { this._l.message = [f]; }
  fire(n, ev) { (this._l[n] ?? []).forEach((f) => f(ev)); }
  send() {} close() {}
};

let failed = 0;
const check = (l, ok, d) => { console.log(`  ${ok ? "ok  " : "FAIL"}  ${l}`); if (!ok) { failed++; if (d) console.log("        " + d); } };

try { new Function(js)(); check("the page's script runs without throwing", true); }
catch (e) { check("the page's script runs without throwing", false, e.message); process.exit(1); }

const sock = sockets[0];
check("it opened a websocket on load", !!sock);

try {
  sock.fire("open", {});
  sock.fire("message", { data: boardMsg });
  check("a real board message renders without throwing", true);
} catch (e) { check("a real board message renders without throwing", false, e.stack.split("\n").slice(0,3).join(" | ")); }

const parsed = JSON.parse(boardMsg);

// The contract's grouping, restated here so the page cannot quietly change it.
// Three tables, in this order; `clear` is in none of them and is only drawn at
// all when the toggle has asked sauron to send those rows.
const GROUP = {
  errored: "your-move", blocked: "your-move", ack: "your-move",
  "needs-test": "awaiting-testing",
  working: "working", delegated: "working", stalled: "working",
  clear: "clear",
};
const WORD = {
  "your-move": "YOUR MOVE",
  "awaiting-testing": "AWAITING TESTING",
  working: "WORKING",
  clear: "CLEAR",
};

const pane = (cls) => byId.board.children.find((c) => c.className === cls);
const tablesOf = () => (pane("tables") ? pane("tables").children : [])
  .filter((c) => c.className && c.className.startsWith("tbl "));
const bodyOf = (t) => (t.children.find((c) => c.className === "rows") || { children: [] }).children;
const rowsOf = () => tablesOf().flatMap(bodyOf);

// Everything the layout promises, asserted against whatever board is on screen.
// Run twice: once on the live message, and once on a board carrying every
// status at once, which one repo's sessions will not do on their own.
function structure(msg, tag) {
  const of = (id) => msg.rows.find((r) => r.id === id);
  const tables = tablesOf();
  const rows = rowsOf();

  check(`${tag}: one line per agent (${msg.rows.length} rows)`, rows.length === msg.rows.length,
    `rendered ${rows.length} rows in ${tables.length} tables`);
  check(`${tag}: rows are grouped into tables, not one flat list`, tables.length >= 1);

  // The whole point of the layout: a row is one line, so the columns line up
  // down the table. Anything that would grow a row -- the prompt, the file
  // list, the buttons -- belongs to the detail pane now.
  check(`${tag}: no row carries the detail that would make it taller than a line`,
    rows.every((r) => !r.children.some((c) => ["said", "files", "acts"].includes(c.className))));
  check(`${tag}: every row has exactly the cells its table's header names`,
    tables.every((t) => {
      const hd = t.children.find((c) => c.className === "hd");
      return hd && bodyOf(t).every((r) => r.children.length === hd.children.length);
    }));

  // Grouping is decided in Rust and arrives as `group`; this asserts the page
  // put each row where the contract says, under the contract's word for it.
  check(`${tag}: each table holds exactly the statuses the contract gives it, under its word`,
    tables.every((t) => {
      const key = t.className.slice(4).trim();
      const mine = bodyOf(t).map((n) => of(n.dataset.id));
      const want = msg.rows.filter((r) => GROUP[r.status] === key);
      const head = t.children.find((c) => c.tagName === "h2");
      return mine.length === want.length
        && mine.every((r, i) => r && r.id === want[i].id)
        && head && head.textContent === `${WORD[key]} (${want.length})`;
    }), tables.map((t) => `${t.className}: ${(t.children.find((c) => c.tagName === "h2") || {}).textContent}`).join(" | "));
  check(`${tag}: a stalled agent is listed as working, not as something owed to you`,
    !msg.rows.some((r) => r.status === "stalled")
    || tables.some((t) => t.className === "tbl working"
      && bodyOf(t).some((n) => of(n.dataset.id).status === "stalled")));

  // --- the detail pane, which is where a row's detail went ----------------
  check(`${tag}: the selected row's detail carries actions you can click`,
    !rows.length || (pane("detail") && pane("detail").children.some((c) => c.className === "acts" && c.children.length > 0)),
    pane("detail") && pane("detail").children.map((c) => c.className).join(","));
  check(`${tag}: the detail pane is showing the selected row`,
    !rows.length || pane("detail").children.some((c) =>
      c.tagName === "h3" && c.textContent === of(rows[0].dataset.id).name));
  if (rows.length > 1) {
    // Selecting another row has to move the detail with it -- the list is read
    // by moving down it, and the pane is what makes that worth doing.
    const last = rowsOf()[rows.length - 1];
    const target = of(last.dataset.id);
    last.onclick();
    check(`${tag}: selecting a row moves the detail to that row`,
      pane("detail").children.some((c) => c.tagName === "h3" && c.textContent === target.name),
      `detail is showing ${(pane("detail").children.find((c) => c.tagName === "h3") || {}).textContent}`);
    check(`${tag}: exactly one row is marked as the selected one`,
      rowsOf().filter((r) => r.className.includes(" on")).length === 1);
  }

  // --- the servant colour, from the wire to the screen --------------------
  // The colour is a pure function of the session id (`servant.rs`) and is sent
  // with every row and every tab. It is worth nothing until something is
  // painted with it, and every one has been silently missing at some point.
  check(`${tag}: each row carries its servant colour`, rows.length > 0 && rows.every((r) => {
    const set = r.style.props["--servant"];
    return set && /^rgb\(\d+,\d+,\d+\)$/.test(set);
  }), `--servant on the rows: ${rows.map((r) => r.style.props["--servant"]).join(" ")}`);
  check(`${tag}: a row's colour is the one sauron sent for that row`, rows.every((r) =>
    r.style.props["--servant"] === `rgb(${of(r.dataset.id).color.join(",")})`));
}

check(`the header names the repo (${parsed.repo})`, byId.repo.textContent === parsed.repo);
structure(parsed, "live board");
check("the tab strip drew the board tab and the +", byId.tabs.children.length >= 2);

// --- every status at once -------------------------------------------------
// One repo's sessions will not be errored, blocked, stalled and delegated on
// the same tick, so the tables above are mostly untested against a live board.
// This is a fixture in the shape of the real message -- the live row, cloned
// per status with the fields `web/json.rs` fills in -- which is the only way to
// see all three tables drawn at once. It asserts layout, not vocabulary: the
// Rust tests own which status is in which table.
const TEMPLATE = parsed.rows[0] || {
  id: "seed", short: "seed", name: "a task", status: "working", statusLabel: "working",
  tag: "working", group: "working", why: "working", doing: "json.rs", orc: false,
  edits: 0, tokens: 0, tokensText: "", lastActivity: 0, ago: "1m", turnStarted: 0,
  pending: [], continueCmd: "claude --resume seed", servant: "seed", color: [200, 120, 90],
};
const every = Object.entries(GROUP).map(([status, group], i) => ({
  ...TEMPLATE,
  id: `fixture-${status}`, short: status, name: `${status} session`,
  status, statusLabel: status, tag: status, group, why: status,
  doing: status, ago: `${i + 1}m`, elapsed: `${i + 1}m 10s`, startedAt: "3:42 PM",
  tokens: i * 1000, tokensText: i ? `${i}k` : "",
  pending: i % 2 ? [`src/web/thing${i}.rs`] : [],
  color: [90 + i * 12, 140, 220 - i * 9],
}));
sock.fire("message", { data: JSON.stringify({ ...parsed, rows: every, showClear: true }) });
check("a board carrying every status renders without throwing", true);
check("the selection falls back to the first row when the one it was on went away",
  rowsOf().length > 0 && rowsOf()[0].className.includes(" on"));
structure({ ...parsed, rows: every }, "every status");
check("all three tables are drawn, plus clear once it is asked for",
  tablesOf().map((t) => t.className.slice(4).trim()).join(",") === "your-move,awaiting-testing,working,clear",
  tablesOf().map((t) => t.className).join(" | "));

// Back to the real board, so what follows sees the state the page shipped with.
sock.fire("message", { data: boardMsg });

const TAB = { id: 1, title: "gimli", kind: "agent", session: "x8f21ba0c", color: [205, 165, 255], dead: false };
const PURPLE = `rgb(${TAB.color.join(",")})`;
try { sock.fire("message", { data: JSON.stringify({ t: "tabs", tabs: [TAB] }) });
  check("a tab message renders a tab", byId.tabs.children.length >= 3); }
catch (e) { check("a tab message renders a tab", false, e.message); }

const tabNode = byId.tabs.children.find((t) => t.style.props["--dot"] === PURPLE);
check("the tab strip paints the tab in its servant colour", !!tabNode,
  `--dot values: ${byId.tabs.children.map((t) => t.style.props["--dot"]).join(" ")}`);

// Opening the panel is what the user does after clicking a tab, and the panel
// is the surface they then stare at for an hour. It has to be the same colour.
sock.fire("message", { data: JSON.stringify({ t: "opened", pane: TAB.id }) });
const panel = byId.terms.children.find((n) => n.className === "term");
check("opening a pane builds a panel", !!panel);
check("the panel is tinted with its servant colour", !!panel && panel.style.props["--servant"] === PURPLE,
  panel && `--servant is ${panel.style.props["--servant"]}`);
check("the panel names its servant in its header", !!panel &&
  panel.children.some((h) => h.tagName === "header" && h.children.some((k) => k.textContent === TAB.title)));
const term = spawned[spawned.length - 1];
check("the terminal's own cursor is the servant colour", !!term && term.options.theme.cursor === PURPLE,
  term && `cursor is ${term.options.theme && term.options.theme.cursor}`);
check("the terminal's background is tinted, not the flat panel black", !!term &&
  term.options.theme.background !== "#0a0c10" && /^#[0-9a-f]{6}$/.test(term.options.theme.background),
  term && `background is ${term.options.theme.background}`);

console.log(failed ? `\n${failed} failed` : "\nall good");
process.exit(failed ? 1 : 0);
