"use strict";

// The only module in this package that opens a socket.
//
// Everything else — `platform.js`, `release.js`, `locate.js`, the shims — is
// pure string and filesystem work, and that separation is the point: it lets
// every naming, parsing and installation rule be tested without a server, and
// it makes "does this package reach the network?" answerable by looking at one
// file. `release.js` names the URLs; this module fetches them; `install.js`
// (T22-08) decides what to trust and where to put it.
//
// WHAT THIS MODULE DELIBERATELY DOES NOT DO
//
// It does not compare digests. It computes a sha256 while streaming and hands
// it back; the comparison against the published checksum belongs to the caller,
// which is what keeps the retry loop *below* the trust decision — a mismatched
// download must never be retried into acceptance (ADR-0013 §Decision 2).
//
// It does not rename, fsync a directory, chmod, or clean up a failed partial.
// Those are `install.js`'s, mirroring `crates/models/src/install.rs`, where the
// fetcher writes and the installer decides. It does fsync the *file* before
// returning, because the digest is compared after durability, not before.
//
// It never sets `Authorization`. The repository is public; sending a token
// would be a way to leak one, not a way to succeed.
//
// It does not honour `npm_config_strict_ssl=false`. Turning off certificate
// verification is not a supported way to install this package; an air-gapped
// or proxied install is served by `LOCAL_RAG_BIN_DIR` and the proxy settings
// below instead.
//
// TLS IS NOT COVERED BY THE TESTS. The fixture server is plain HTTP on
// loopback, so the `https:` path and the `CONNECT` tunnel are exercised only as
// far as "what did the client ask for" — see `test/http-proxy.test.js`. That is
// a deliberate scope line, not an oversight: standing up a TLS fixture would
// mean shipping a certificate in the repository.
//
// RETRIES AND TIMEOUTS ARE NEW POLICY, not a port of the Rust side. The Rust
// fetcher (`crates/models/src/fetch.rs`) has neither: it takes ureq's defaults.
// Here they are explicit, because an npm `postinstall` runs on machines whose
// networks nobody has ever seen.

const http = require("node:http");
const https = require("node:https");
const fs = require("node:fs");
const crypto = require("node:crypto");

const { parseTagFromLocation, latestAssetUrl } = require("./release");

const DEFAULT_MAX_REDIRECTS = 5;
const DEFAULT_RETRIES = 3;
const DEFAULT_HEADER_TIMEOUT_MS = 30_000;
const DEFAULT_IDLE_TIMEOUT_MS = 60_000;
const DEFAULT_MAX_STRING_BYTES = 64 * 1024;
const RETRY_BACKOFF_MS = [500, 1500, 4000];

/** Errors carry a kind so the caller can tell a 404 from a dead network. */
class HttpError extends Error {
  /** @param {"transport"|"status"|"io"|"protocol"} kind */
  constructor(kind, message, { url, status } = {}) {
    super(message);
    this.name = "HttpError";
    this.kind = kind;
    this.url = url;
    this.status = status;
  }
}

// ---------------------------------------------------------------------------
// Proxy selection — a pure function, and the part most likely to be wrong.
// ---------------------------------------------------------------------------

/**
 * Which proxy, if any, applies to `url`.
 *
 * Precedence runs most-specific to least: this project's own override, then
 * npm's configuration (the `formatDownloadError` message promises npm's
 * settings are honoured, so they are not optional), then the conventional
 * environment. `NO_PROXY` wins over all of them, and loopback is always
 * bypassed — a fixture server on 127.0.0.1 must not be dragged through a
 * developer's corporate proxy.
 *
 * @param {string} url
 * @param {NodeJS.ProcessEnv} [env]
 * @returns {string|null}
 */
function proxyForUrl(url, env = process.env) {
  let target;
  try {
    target = new URL(url);
  } catch {
    return null;
  }
  // `URL` keeps IPv6 hosts bracketed (`[::1]`), so strip them before comparing.
  const host = target.hostname.toLowerCase().replace(/^\[|\]$/g, "");

  if (host === "localhost" || host === "127.0.0.1" || host === "::1") {
    return null;
  }

  const noProxy = firstSet(env, ["NO_PROXY", "no_proxy", "npm_config_noproxy"]);
  if (noProxy && noProxyMatches(noProxy, host, target.port)) {
    return null;
  }

  const secure = target.protocol === "https:";
  const names = secure
    ? [
        "LOCAL_RAG_HTTPS_PROXY",
        "npm_config_https_proxy",
        "npm_config_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
      ]
    : [
        "LOCAL_RAG_HTTPS_PROXY",
        "npm_config_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
      ];
  return firstSet(env, names);
}

function firstSet(env, names) {
  for (const n of names) {
    const v = env[n];
    if (typeof v === "string" && v.trim().length > 0) {
      return v.trim();
    }
  }
  return null;
}

/** `NO_PROXY` is a comma list of suffixes; `*` disables proxying entirely. */
function noProxyMatches(noProxy, host, port) {
  for (const raw of noProxy.split(",")) {
    const entry = raw.trim().toLowerCase();
    if (entry.length === 0) continue;
    if (entry === "*") return true;
    const [pattern, entryPort] = splitHostPort(entry);
    if (entryPort && entryPort !== String(port)) continue;
    const suffix = pattern.startsWith(".") ? pattern.slice(1) : pattern;
    if (host === suffix || host.endsWith(`.${suffix}`)) return true;
  }
  return false;
}

function splitHostPort(entry) {
  const idx = entry.lastIndexOf(":");
  if (idx > 0 && /^\d+$/.test(entry.slice(idx + 1))) {
    return [entry.slice(0, idx), entry.slice(idx + 1)];
  }
  return [entry, null];
}

// ---------------------------------------------------------------------------
// One request, no redirect following, no retry.
// ---------------------------------------------------------------------------

function requestOnce(url, opts) {
  const env = opts.env ?? process.env;
  const target = new URL(url);
  // `opts.proxy` bypasses selection entirely. It exists because selection
  // deliberately never proxies loopback, which would otherwise make the proxy
  // *transport* untestable against a local fixture — the same reason every
  // other function here takes `env` rather than reading it globally.
  const proxy = opts.proxy ?? proxyForUrl(url, env);
  const secure = target.protocol === "https:";

  return new Promise((resolve, reject) => {
    /** @type {import('node:http').ClientRequestArgs} */
    let requestOptions;
    let mod;

    if (proxy && !secure) {
      // Plain HTTP through a proxy is an absolute-form request line; no tunnel.
      const p = new URL(proxy);
      mod = http;
      requestOptions = {
        host: p.hostname,
        port: p.port || 80,
        method: "GET",
        path: url,
        headers: { host: target.host },
      };
    } else {
      mod = secure ? https : http;
      requestOptions = {
        host: target.hostname,
        port: target.port || (secure ? 443 : 80),
        method: "GET",
        path: `${target.pathname}${target.search}`,
        headers: {},
      };
      if (secure && proxy) {
        requestOptions.agent = tunnelAgent(proxy, target);
      }
      const cafile = env.npm_config_cafile;
      if (secure && cafile) {
        try {
          requestOptions.ca = fs.readFileSync(cafile);
        } catch {
          // A misconfigured cafile must not be silently swapped for "no
          // verification"; fall through to the system store instead.
        }
      }
    }

    const req = mod.request(requestOptions, (res) => {
      clearTimeout(headerTimer);
      res.setTimeout(opts.idleTimeoutMs ?? DEFAULT_IDLE_TIMEOUT_MS, () => {
        res.destroy(
          new HttpError("transport", `idle for too long while reading ${url}`, { url }),
        );
      });
      resolve(res);
    });

    const headerTimer = setTimeout(() => {
      req.destroy(
        new HttpError("transport", `no response headers from ${url} in time`, { url }),
      );
    }, opts.headerTimeoutMs ?? DEFAULT_HEADER_TIMEOUT_MS);
    headerTimer.unref?.();

    req.on("error", (err) => {
      clearTimeout(headerTimer);
      reject(
        err instanceof HttpError
          ? err
          : new HttpError("transport", `${err.message} (${url})`, { url }),
      );
    });
    req.end();
  });
}

/**
 * A `CONNECT` tunnel through `proxy` to `target`. Only reached for `https:`
 * origins — this is what the card means by proxy support, and it is the one
 * path the loopback fixture cannot exercise end to end.
 */
function tunnelAgent(proxy, target) {
  const p = new URL(proxy);
  const agent = new https.Agent({ keepAlive: false });
  const original = agent.createConnection.bind(agent);
  agent.createConnection = (options, callback) => {
    const connectReq = http.request({
      host: p.hostname,
      port: p.port || 80,
      method: "CONNECT",
      path: `${target.hostname}:${target.port || 443}`,
      headers: { host: `${target.hostname}:${target.port || 443}` },
    });
    connectReq.on("connect", (_res, socket) => {
      callback(null, original({ ...options, socket }, undefined));
    });
    connectReq.on("error", (err) => callback(err));
    connectReq.end();
  };
  return agent;
}

async function requestFollowing(url, opts) {
  const max = opts.maxRedirects ?? DEFAULT_MAX_REDIRECTS;
  let current = url;
  for (let hop = 0; hop <= max; hop += 1) {
    const res = await requestOnce(current, opts);
    const status = res.statusCode ?? 0;
    if (status >= 300 && status < 400 && res.headers.location) {
      res.resume();
      const next = new URL(res.headers.location, current);
      if (next.protocol !== "http:" && next.protocol !== "https:") {
        throw new HttpError("protocol", `refusing to follow ${next.protocol} redirect`, {
          url: current,
        });
      }
      current = next.toString();
      continue;
    }
    return { res, finalUrl: current };
  }
  throw new HttpError("protocol", `more than ${max} redirects starting at ${url}`, { url });
}

function isRetryable(err) {
  if (!(err instanceof HttpError)) return false;
  if (err.kind === "transport") return true;
  if (err.kind === "status") {
    return err.status === 429 || (err.status >= 500 && err.status <= 599);
  }
  return false;
}

async function withRetries(attempt, opts) {
  const tries = opts.retries ?? DEFAULT_RETRIES;
  let last;
  for (let i = 0; i < tries; i += 1) {
    try {
      return await attempt();
    } catch (err) {
      last = err;
      if (!isRetryable(err) || i === tries - 1) throw err;
      const wait = RETRY_BACKOFF_MS[Math.min(i, RETRY_BACKOFF_MS.length - 1)];
      await new Promise((r) => setTimeout(r, opts.backoff === false ? 0 : wait));
    }
  }
  throw last;
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/**
 * The tag `latest` currently points at, read from the redirect's `Location`
 * **without following it**: the answer is in the first hop's headers, so no
 * payload needs to move and no rate-limited API call is spent.
 *
 * @param {string} asset @param {object} [opts] @returns {Promise<string>}
 */
async function resolveLatestTag(asset, opts = {}) {
  const url = latestAssetUrl(asset, opts.env ?? process.env);
  return withRetries(async () => {
    const res = await requestOnce(url, opts);
    const status = res.statusCode ?? 0;
    const location = res.headers.location;
    res.resume();
    res.destroy();
    if (status < 300 || status >= 400 || !location) {
      throw new HttpError("status", `${url} did not redirect to a tag (HTTP ${status})`, {
        url,
        status,
      });
    }
    return parseTagFromLocation(new URL(location, url).toString());
  }, opts);
}

/**
 * Fetch a small body — a checksum sidecar — with a hard ceiling. The ceiling
 * aborts the stream when it is crossed rather than buffering first and checking
 * after, so a hostile or broken server cannot make this allocate without bound.
 *
 * @param {string} url @param {object} [opts] @returns {Promise<string>}
 */
async function httpGetToString(url, opts = {}) {
  const max = opts.maxBytes ?? DEFAULT_MAX_STRING_BYTES;
  return withRetries(
    () =>
      new Promise((resolve, reject) => {
        requestFollowing(url, opts).then(({ res, finalUrl }) => {
          const status = res.statusCode ?? 0;
          if (status !== 200) {
            res.resume();
            reject(new HttpError("status", `HTTP ${status} for ${finalUrl}`, { url: finalUrl, status }));
            return;
          }
          const chunks = [];
          let seen = 0;
          res.on("data", (c) => {
            seen += c.length;
            if (seen > max) {
              res.destroy();
              reject(new HttpError("protocol", `${finalUrl} exceeded ${max} bytes`, { url: finalUrl }));
              return;
            }
            chunks.push(c);
          });
          res.on("error", reject);
          res.on("end", () => resolve(Buffer.concat(chunks).toString("utf8")));
        }, reject);
      }),
    opts,
  );
}

/**
 * Stream `url` into `destPath`, hashing the same chunks that are written so the
 * file is never read back to verify it — `HashingWriter`'s shape from
 * `crates/models/src/install.rs`.
 *
 * Each attempt truncates `destPath`. That is what makes a retry safe: a second
 * attempt appending to a first attempt's partial bytes would produce a file
 * whose digest is meaningless, and no comparison downstream could detect that
 * it happened rather than a genuine corruption.
 *
 * @param {string} url @param {string} destPath @param {object} [opts]
 * @returns {Promise<{bytesWritten: number, sha256: string}>}
 */
async function httpGetToFile(url, destPath, opts = {}) {
  return withRetries(
    () =>
      new Promise((resolve, reject) => {
        requestFollowing(url, opts).then(({ res, finalUrl }) => {
          const status = res.statusCode ?? 0;
          if (status !== 200) {
            res.resume();
            reject(new HttpError("status", `HTTP ${status} for ${finalUrl}`, { url: finalUrl, status }));
            return;
          }
          const expected = Number(res.headers["content-length"] ?? NaN);
          const hash = crypto.createHash("sha256");
          let written = 0;
          // "w" truncates: no attempt can ever see a previous attempt's bytes.
          const sink = fs.createWriteStream(destPath, { flags: "w" });
          let settled = false;
          const fail = (err) => {
            if (settled) return;
            settled = true;
            sink.destroy();
            reject(
              err instanceof HttpError
                ? err
                : new HttpError("io", `${err.message} (${finalUrl})`, { url: finalUrl }),
            );
          };
          res.on("data", (c) => {
            written += c.length;
            hash.update(c);
          });
          res.on("error", fail);
          sink.on("error", fail);
          res.pipe(sink);
          sink.on("finish", () => {
            if (settled) return;
            if (Number.isFinite(expected) && written !== expected) {
              fail(
                new HttpError(
                  "transport",
                  `${finalUrl} ended after ${written} of ${expected} bytes`,
                  { url: finalUrl },
                ),
              );
              return;
            }
            // fsync before returning: the digest is compared after durability.
            fs.open(destPath, "r+", (openErr, fd) => {
              if (openErr) return fail(openErr);
              fs.fsync(fd, (syncErr) => {
                fs.close(fd, () => {
                  if (syncErr) return fail(syncErr);
                  settled = true;
                  resolve({ bytesWritten: written, sha256: hash.digest("hex") });
                });
              });
            });
          });
        }, reject);
      }),
    opts,
  );
}

module.exports = {
  HttpError,
  proxyForUrl,
  resolveLatestTag,
  httpGetToString,
  httpGetToFile,
  DEFAULT_MAX_STRING_BYTES,
};
