#!/usr/bin/env node
// End to end: does a browser get a board, open a real agent, and talk to it?
//
// This exercises the parts no Rust unit test can reach -- the websocket
// handshake, a pty actually spawning under a login shell, keystrokes arriving at
// its tty, and its output coming back on the socket. It drives a real
// `sauron serve` over a real socket and asserts on what comes out.
//
// Run, from the sauron/ directory:
//   cargo build --release && node tests/web_workspace.mjs
//
// Not wired into `cargo test`: that would make the Rust suite depend on a node
// install, and a test that skips itself when the runtime is missing goes green
// by not running.

import { spawn } from "node:child_process";

const PORT = 7405;
const BIN = "target/release/sauron";
const wait = (ms) => new Promise((r) => setTimeout(r, ms));

let failed = 0;
const check = (label, ok, detail) => {
  console.log(`  ${ok ? "ok  " : "FAIL"}  ${label}`);
  if (!ok) { failed++; if (detail) console.log(`        ${detail}`); }
};

const server = spawn(BIN, ["serve", "--port", String(PORT), "--agents", "0"], { stdio: "ignore" });
process.on("exit", () => server.kill("SIGKILL"));
await wait(2500);

const ws = new WebSocket(`ws://127.0.0.1:${PORT}/ws`);
ws.binaryType = "arraybuffer";

const seen = { board: null, tabs: [], opened: null, notices: [] };
let ptyText = "";

ws.addEventListener("message", (e) => {
  if (typeof e.data !== "string") {
    const buf = new Uint8Array(e.data);
    ptyText += new TextDecoder().decode(buf.subarray(1));
    return;
  }
  const m = JSON.parse(e.data);
  if (m.t === "board") seen.board = m;
  else if (m.t === "tabs") seen.tabs = m.tabs;
  else if (m.t === "opened") seen.opened = m.pane;
  else if (m.t === "notice") seen.notices.push(m.text);
});

await new Promise((res, rej) => {
  ws.addEventListener("open", res);
  ws.addEventListener("error", rej);
  setTimeout(() => rej(new Error("websocket never opened")), 5000);
});

const send = (o) => ws.send(JSON.stringify(o));
const keys = (pane, s) => {
  const b = new TextEncoder().encode(s);
  const f = new Uint8Array(b.length + 1);
  f[0] = pane;
  f.set(b, 1);
  ws.send(f);
};

console.log("web workspace:\n");

// --- the board arrives unasked -------------------------------------------
await wait(400);
check("a page gets the board without asking for it", seen.board !== null);
check("the board names the repo it is watching",
  seen.board && seen.board.repo === "sauron" || seen.board?.repo === "agentwatch",
  `repo was ${seen.board && seen.board.repo}`);
check("rows carry a servant name and colour, so tabs and cards can agree",
  !seen.board.rows.length || (seen.board.rows[0].servant && Array.isArray(seen.board.rows[0].color)));
check("rows carry sauron's own formatting, not raw epochs to re-derive",
  !seen.board.rows.length || typeof seen.board.rows[0].ago === "string");
check("the tab strip starts empty (--agents 0)", seen.tabs.length === 0);

// --- open a real pty ------------------------------------------------------
send({ t: "open", kind: "shell", cols: 100, rows: 30 });
await wait(1800);
check("opening a shell produced a tab", seen.tabs.length === 1, JSON.stringify(seen.tabs));
check("the server told us which pane it is", seen.opened !== null);
const pane = seen.opened;

// --- talk to it -----------------------------------------------------------
ptyText = "";
keys(pane, "echo hello-from-the-browser\n");
await wait(1500);
check("keystrokes reached the tty and its output came back",
  ptyText.includes("hello-from-the-browser"),
  JSON.stringify(ptyText.slice(-160)));

// --- the pty is a real terminal, not a pipe -------------------------------
ptyText = "";
keys(pane, "tty; echo COLS=$(tput cols)\n");
await wait(1500);
check("the child is on a tty, not a pipe", /\/dev\/(tty|pts)/.test(ptyText), JSON.stringify(ptyText.slice(-200)));
check("the tty has the geometry the browser asked for", ptyText.includes("COLS=100"), JSON.stringify(ptyText.slice(-200)));

// --- resize reaches the child --------------------------------------------
send({ t: "resize", pane, cols: 132, rows: 40 });
await wait(400);
ptyText = "";
keys(pane, "echo COLS=$(tput cols)\n");
await wait(1200);
check("a browser resize reflows the child's terminal", ptyText.includes("COLS=132"), JSON.stringify(ptyText.slice(-200)));

// --- scrollback survives a reattach --------------------------------------
ptyText = "";
send({ t: "attach", pane });
await wait(800);
check("attaching replays what the tab missed", ptyText.includes("hello-from-the-browser"),
  `replayed ${ptyText.length} bytes`);

// --- the agent outlives the socket ---------------------------------------
ws.close();
await wait(600);
const ws2 = new WebSocket(`ws://127.0.0.1:${PORT}/ws`);
ws2.binaryType = "arraybuffer";
let tabs2 = [];
ws2.addEventListener("message", (e) => {
  if (typeof e.data === "string") {
    const m = JSON.parse(e.data);
    if (m.t === "tabs") tabs2 = m.tabs;
  }
});
await new Promise((res, rej) => {
  ws2.addEventListener("open", res);
  setTimeout(() => rej(new Error("second socket never opened")), 5000);
});
await wait(600);
check("closing the tab did not kill the agent", tabs2.length === 1, JSON.stringify(tabs2));

// --- closing a tab does ---------------------------------------------------
ws2.send(JSON.stringify({ t: "close", pane }));
await wait(700);
check("closing the tab explicitly ends it", tabs2.length === 0, JSON.stringify(tabs2));

ws2.send(JSON.stringify({ t: "quit" }));
await wait(600);
check("quit stops the server", server.exitCode !== null || server.killed || !(await alive(PORT)));

async function alive(port) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(500) });
    return r.ok;
  } catch { return false; }
}

console.log(failed ? `\n${failed} failed` : "\nall good");
process.exit(failed ? 1 : 0);
