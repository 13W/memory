"use strict";

// A real HTTP server on 127.0.0.1 that serves release-shaped responses, so the
// production client in `src/http.js` is exercised over a real socket without
// ever leaving the machine. `CLAUDE.md` forbids tests that depend on network
// access; ADR-0005 and `crates/models/tests/support/mod.rs` established the
// same loopback-fixture shape on the Rust side, and this is its Node sibling.
//
// Two things are copied from that precedent deliberately. The port is always
// ephemeral (`:0`), never a constant, so parallel runs cannot collide. And every
// request is recorded in order, because "downloaded exactly once" and "retried
// rather than restarted" are claims about *how many* requests happened — the
// only way to assert them is to count, not to infer.
//
// Inert on require: it starts nothing until `startFixtureRelease()` is called.

const http = require("node:http");

/**
 * @typedef {object} FixtureFault
 * @property {number} [status] answer with this status instead of 200
 * @property {number} [failTimes] fail this many times with `status`, then succeed
 * @property {boolean} [truncate] send half the body, then destroy the socket
 * @property {number} [trickleMs] delay between the two halves of the body
 * @property {string} [redirectTo] answer 302 to this absolute URL
 */

/**
 * @param {object} opts
 * @param {string} [opts.tag] the tag `/latest/download/*` redirects to
 * @param {Record<string, string|Buffer>} [opts.assets] path suffix -> body
 * @param {Record<string, FixtureFault>} [opts.faults] same keys as `assets`
 * @returns {Promise<{origin: string, requests: () => object[], requestCount: () => number, close: () => Promise<void>}>}
 */
async function startFixtureRelease(opts = {}) {
  const tag = opts.tag ?? "1.0.0";
  const assets = opts.assets ?? {};
  const faults = opts.faults ?? {};
  /** @type {{method: string, url: string, headers: object}[]} */
  const log = [];
  const failuresSoFar = new Map();

  const server = http.createServer((req, res) => {
    log.push({ method: req.method, url: req.url, headers: { ...req.headers } });

    // `/latest/download/<asset>` is a 302 whose Location names the tag — the
    // shape the real release uses, and the reason `resolveLatestTag` can learn
    // the tag without spending an API call.
    const latest = /^\/latest\/download\/(.+)$/.exec(req.url);
    if (latest) {
      res.writeHead(302, {
        location: `${origin()}/download/${tag}/${latest[1]}`,
        "content-length": "0",
      });
      res.end();
      return;
    }

    const asset = /^\/download\/[^/]+\/(.+)$/.exec(req.url);
    const key = asset ? asset[1] : req.url;
    const fault = faults[key];

    if (fault && fault.redirectTo !== undefined) {
      res.writeHead(302, { location: fault.redirectTo, "content-length": "0" });
      res.end();
      return;
    }

    if (fault && fault.status !== undefined) {
      const seen = failuresSoFar.get(key) ?? 0;
      const budget = fault.failTimes ?? Infinity;
      if (seen < budget) {
        failuresSoFar.set(key, seen + 1);
        res.writeHead(fault.status, { "content-length": "0" });
        res.end();
        return;
      }
    }

    const body = assets[key];
    if (body === undefined) {
      res.writeHead(404, { "content-length": "0" });
      res.end();
      return;
    }
    const buf = Buffer.isBuffer(body) ? body : Buffer.from(body, "utf8");

    if (fault && fault.truncate) {
      // Claim the full length, send half, then cut the connection: the client
      // must notice the short read rather than accept a truncated file.
      // `failTimes` bounds it the same way it bounds a status fault, so a test
      // can say "truncate once, then serve properly" and observe the retry.
      const seen = failuresSoFar.get(`truncate:${key}`) ?? 0;
      if (seen < (fault.failTimes ?? Infinity)) {
        failuresSoFar.set(`truncate:${key}`, seen + 1);
        res.writeHead(200, { "content-length": String(buf.length) });
        res.write(buf.subarray(0, Math.floor(buf.length / 2)));
        res.socket.destroy();
        return;
      }
    }

    if (fault && fault.trickleMs) {
      res.writeHead(200, { "content-length": String(buf.length) });
      const half = Math.floor(buf.length / 2);
      res.write(buf.subarray(0, half));
      setTimeout(() => res.end(buf.subarray(half)), fault.trickleMs).unref();
      return;
    }

    res.writeHead(200, { "content-length": String(buf.length) });
    res.end(buf);
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const origin = () => `http://127.0.0.1:${server.address().port}`;

  return {
    origin: origin(),
    requests: () => log.slice(),
    requestCount: () => log.length,
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections?.();
        server.close(() => resolve());
      }),
  };
}

/**
 * A forward proxy that records what it was asked for. It answers plain-HTTP
 * requests (absolute-form request line) by fetching them itself, and answers
 * `CONNECT` by recording the target and closing — enough to assert *what the
 * client asked to tunnel*, which is the part that can be wrong, without
 * standing up TLS.
 *
 * @returns {Promise<{origin: string, absoluteFormTargets: () => string[], connectTargets: () => string[], close: () => Promise<void>}>}
 */
async function startRecordingProxy() {
  /** @type {string[]} */ const absolute = [];
  /** @type {string[]} */ const connects = [];

  const server = http.createServer((req, res) => {
    absolute.push(req.url);
    // Absolute-form means the client treated us as a proxy. Fetch it for real
    // so the end-to-end path is genuinely exercised.
    const upstream = http.request(req.url, { method: req.method }, (up) => {
      res.writeHead(up.statusCode, up.headers);
      up.pipe(res);
    });
    upstream.on("error", () => {
      res.writeHead(502, { "content-length": "0" });
      res.end();
    });
    req.pipe(upstream);
  });

  server.on("connect", (req, socket) => {
    connects.push(req.url);
    socket.end("HTTP/1.1 200 Connection Established\r\n\r\n");
    socket.destroy();
  });

  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));

  return {
    origin: `http://127.0.0.1:${server.address().port}`,
    absoluteFormTargets: () => absolute.slice(),
    connectTargets: () => connects.slice(),
    close: () =>
      new Promise((resolve) => {
        server.closeAllConnections?.();
        server.close(() => resolve());
      }),
  };
}

module.exports = { startFixtureRelease, startRecordingProxy };
