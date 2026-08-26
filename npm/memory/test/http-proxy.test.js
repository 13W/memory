"use strict";

// Proxy support, split by what can honestly be tested.
//
// Selection — which variable wins, and whether NO_PROXY bypasses — is a pure
// function, and it is where the real bugs live. It is covered exhaustively here
// without a socket.
//
// Transport for `http:` is an absolute-form request line to the proxy, and a
// real local forward proxy exercises it end to end.
//
// Transport for `https:` is a CONNECT tunnel, and a full test needs TLS, which
// is out of scope for this card — a TLS fixture would mean shipping a
// certificate in the repository. What IS tested is the part that can be wrong:
// that a CONNECT is issued at all, and that its target is the origin host and
// port rather than the proxy's. The handshake after that is expected to fail
// against a recording proxy, so the assertion is on what was recorded.
//
// Every case passes an explicit `env`. A developer with HTTPS_PROXY exported
// must get the same result as one without — `CLAUDE.md` forbids tests that
// depend on the environment, and proxy variables are exactly that.

const { test } = require("node:test");
const assert = require("node:assert/strict");

const { startFixtureRelease, startRecordingProxy } = require("./helpers/fixture-server.js");
const { proxyForUrl, httpGetToString } = require("../src/http.js");

const P = "http://proxy.example.test:8080";

test("this project's own override outranks every other source", () => {
  assert.equal(
    proxyForUrl("https://example.test/a", {
      LOCAL_RAG_HTTPS_PROXY: "http://mine:1",
      npm_config_https_proxy: "http://npm:2",
      HTTPS_PROXY: "http://env:3",
      ALL_PROXY: "http://all:4",
    }),
    "http://mine:1",
  );
});

test("npm's own settings are honoured, because the error message promises they are", () => {
  // `formatDownloadError` tells the user "the npm proxy settings are honoured".
  // That sentence shipped in T22-05; this is what makes it true rather than a
  // claim.
  assert.equal(
    proxyForUrl("https://example.test/a", {
      npm_config_https_proxy: "http://npm:2",
      HTTPS_PROXY: "http://env:3",
    }),
    "http://npm:2",
  );
  assert.equal(
    proxyForUrl("https://example.test/a", {
      npm_config_proxy: "http://npmplain:2",
      HTTPS_PROXY: "http://env:3",
    }),
    "http://npmplain:2",
  );
});

test("http: and https: consult their own variables, and ALL_PROXY is the fallback", () => {
  assert.equal(proxyForUrl("http://example.test/a", { HTTP_PROXY: P }), P);
  assert.equal(proxyForUrl("http://example.test/a", { HTTPS_PROXY: P }), null);
  assert.equal(proxyForUrl("https://example.test/a", { ALL_PROXY: P }), P);
  assert.equal(proxyForUrl("http://example.test/a", { all_proxy: P }), P);
});

test("NO_PROXY beats every proxy variable", () => {
  const env = { HTTPS_PROXY: P, ALL_PROXY: P, LOCAL_RAG_HTTPS_PROXY: P };
  assert.equal(proxyForUrl("https://example.test/a", { ...env, NO_PROXY: "*" }), null);
  assert.equal(proxyForUrl("https://example.test/a", { ...env, NO_PROXY: "example.test" }), null);
  assert.equal(proxyForUrl("https://a.example.test/x", { ...env, NO_PROXY: ".example.test" }), null);
  assert.equal(proxyForUrl("https://a.example.test/x", { ...env, no_proxy: "example.test" }), null);
  assert.equal(proxyForUrl("https://a.example.test/x", { ...env, npm_config_noproxy: "example.test" }), null);
  // A suffix must not match a different domain that merely ends the same way.
  assert.equal(proxyForUrl("https://notexample.test/x", { ...env, NO_PROXY: "example.test" }), P);
});

test("a NO_PROXY entry with a port only bypasses that port", () => {
  const env = { HTTPS_PROXY: P, NO_PROXY: "example.test:8443" };
  assert.equal(proxyForUrl("https://example.test:8443/a", env), null);
  assert.equal(proxyForUrl("https://example.test:9999/a", env), P);
});

test("loopback is never proxied, whatever the environment says", () => {
  for (const host of ["127.0.0.1", "localhost", "[::1]"]) {
    assert.equal(
      proxyForUrl(`http://${host}:8080/a`, { ALL_PROXY: P, HTTPS_PROXY: P }),
      null,
      `${host} must bypass`,
    );
  }
});

test("an unparseable URL yields no proxy rather than throwing", () => {
  assert.equal(proxyForUrl("not a url", { ALL_PROXY: P }), null);
});

test("an http: request through a real proxy arrives in absolute form", async (t) => {
  const server = await startFixtureRelease({ assets: { "a.txt": "hello" } });
  t.after(() => server.close());
  const proxy = await startRecordingProxy();
  t.after(() => proxy.close());

  const url = `${server.origin}/download/1.0.0/a.txt`;
  // proxyForUrl bypasses loopback, so the proxy is forced explicitly here —
  // the transport, not the selection, is what this case is about.
  const body = await httpGetToString(url, {
    env: {},
    proxy: proxy.origin,
    backoff: false,
  });
  assert.equal(body, "hello");
  assert.deepEqual(proxy.absoluteFormTargets(), [url], "the proxy saw the whole URL");
});

test("an https: request through a proxy issues CONNECT to the origin, not the proxy", async (t) => {
  const proxy = await startRecordingProxy();
  t.after(() => proxy.close());

  // The recording proxy accepts the CONNECT and then closes, so the TLS
  // handshake cannot complete. That failure is expected; the assertion is on
  // what the client asked to tunnel.
  await assert.rejects(() =>
    httpGetToString("https://example.test/a", {
      env: { HTTPS_PROXY: proxy.origin },
      backoff: false,
      retries: 1,
    }),
  );
  assert.deepEqual(
    proxy.connectTargets(),
    ["example.test:443"],
    "CONNECT names the origin host and port",
  );
});
