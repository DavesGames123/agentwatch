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
    children: [], style: { setProperty() {} },
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
globalThis.Terminal = class { constructor() {} loadAddon() {} open() {} onData() {} write() {} focus() {} reset() {} dispose() {} get cols(){return 80} get rows(){return 24} };
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
const bands = byId.board.children.filter((c) => c.className === "band");
const cards = bands.flatMap((b) => b.children).filter((c) => c.className && c.className.startsWith("cards"))
  .flatMap((g) => g.children);
check(`the header names the repo (${parsed.repo})`, byId.repo.textContent === parsed.repo);
check(`one card per row (${parsed.rows.length} rows)`, cards.length === parsed.rows.length,
  `rendered ${cards.length} cards in ${bands.length} bands`);
check("every card carries actions you can click", cards.length > 0 && cards.every((c) =>
  c.children.some((k) => k.className === "acts" && k.children.length > 0)));
check("the tab strip drew the board tab and the +", byId.tabs.children.length >= 2);
check("cards are grouped into attention bands, not one flat list", bands.length >= 1);

try { sock.fire("message", { data: JSON.stringify({ t: "tabs", tabs: [{ id: 1, title: "gimli", kind: "agent", session: "x", color: [205,165,255], dead: false }] }) });
  check("a tab message renders a tab", byId.tabs.children.length >= 3); }
catch (e) { check("a tab message renders a tab", false, e.message); }

console.log(failed ? `\n${failed} failed` : "\nall good");
process.exit(failed ? 1 : 0);
