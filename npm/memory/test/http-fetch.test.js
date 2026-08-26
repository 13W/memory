"use strict";

// Real sockets, loopback only. Every byte here comes from a fixture server on
// 127.0.0.1 (`CLAUDE.md`: tests must not depend on network access), and the
// production client in `src/http.js` is the one under test — not a mock of it.
//
// Servers are torn down with `t.after` rather than a trailing statement, which
// is a deliberate divergence from this suite's usual inline cleanup: a listener
// leaked by a failing assertion keeps the event loop alive and hangs the whole
// run, so teardown has to happen even when the test throws.

const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");

const { mkTmpRoot } = require("./helpers/tmp.js");
const { startFixtureRelease } = require("./helpers/fixture-server.js");
const {
  HttpError,
  resolveLatestTag,
  httpGetToString,
  httpGetToFile,
} = require("../src/http.js");

const BODY = Buffer.from("a".repeat(5000) + "tail", "utf8");
const BODY_SHA = crypto.createHash("sha256").update(BODY).digest("hex");
// Loopback is bypassed by proxyForUrl anyway, but pin it so a developer with an
// ambient proxy configured cannot change what these tests exercise.
const CLEAN_ENV = { NO_PROXY: "*" };

function envFor(origin) {
  return { ...CLEAN_ENV, LOCAL_RAG_RELEASE_BASE_URL: origin };
}

test("the tag is read from the first hop's Location without following it", async (t) => {
  const server = await startFixtureRelease({ tag: "2.3.4", assets: { "a.tar.gz": BODY } });
  t.after(() => server.close());

  const tag = await resolveLatestTag("a.tar.gz", { env: envFor(server.origin) });
  assert.equal(tag, "2.3.4");
  assert.equal(server.requestCount(), 1, "the redirect target must not be fetched");
  assert.match(server.requests()[0].url, /^\/latest\/download\//);
});

test("a redirect chain is followed, and a body arrives with its bytes and digest", async (t) => {
  const server = await startFixtureRelease({ tag: "1.0.0", assets: { "a.tar.gz": BODY } });
  t.after(() => server.close());
  const root = mkTmpRoot("lr-http-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));

  const dest = path.join(root, "a.tar.gz");
  const out = await httpGetToFile(`${server.origin}/latest/download/a.tar.gz`, dest, {
    env: CLEAN_ENV,
  });
  assert.equal(out.bytesWritten, BODY.length);
  assert.equal(out.sha256, BODY_SHA);
  assert.deepEqual(fs.readFileSync(dest), BODY);
});

test("too many redirects is an error, not an infinite walk", async (t) => {
  // `faults` is captured by reference, so the self-referential target can be
  // filled in once the ephemeral port is known.
  const faults = { loop: {} };
  const server = await startFixtureRelease({ assets: {}, faults });
  t.after(() => server.close());
  faults.loop.redirectTo = `${server.origin}/download/1.0.0/loop`;

  await assert.rejects(
    () =>
      httpGetToString(`${server.origin}/download/1.0.0/loop`, {
        env: CLEAN_ENV,
        maxRedirects: 3,
        backoff: false,
      }),
    (err) => err instanceof HttpError && /redirects/.test(err.message),
  );
  assert.equal(server.requestCount(), 4, "the initial request plus three hops, then a stop");
});

test("httpGetToString refuses a body past its ceiling instead of buffering it", async (t) => {
  const server = await startFixtureRelease({ assets: { "big": BODY } });
  t.after(() => server.close());

  await assert.rejects(
    () => httpGetToString(`${server.origin}/download/1.0.0/big`, { env: CLEAN_ENV, maxBytes: 100 }),
    (err) => err instanceof HttpError && /exceeded 100 bytes/.test(err.message),
  );
});

test("a 5xx is retried and then succeeds; the request log proves both attempts", async (t) => {
  const server = await startFixtureRelease({
    assets: { "a.tar.gz": BODY },
    faults: { "a.tar.gz": { status: 500, failTimes: 1 } },
  });
  t.after(() => server.close());
  const root = mkTmpRoot("lr-http-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));

  const dest = path.join(root, "a.tar.gz");
  const out = await httpGetToFile(`${server.origin}/download/1.0.0/a.tar.gz`, dest, {
    env: CLEAN_ENV,
    backoff: false,
  });
  assert.equal(out.sha256, BODY_SHA);
  assert.equal(server.requestCount(), 2, "one failure, one success");
});

test("a 404 is not retried — it is an answer, not a glitch", async (t) => {
  const server = await startFixtureRelease({ assets: {} });
  t.after(() => server.close());

  await assert.rejects(
    () => httpGetToString(`${server.origin}/download/1.0.0/absent`, { env: CLEAN_ENV, backoff: false }),
    (err) => err instanceof HttpError && err.kind === "status" && err.status === 404,
  );
  assert.equal(server.requestCount(), 1, "a 404 must be taken at its word");
});

test("a retry after a truncated body does not concatenate the two attempts", async (t) => {
  // The defect this guards against is specific and silent: if the second
  // attempt appended to the first's partial bytes, the file would be longer
  // than the asset and its digest meaningless — and nothing downstream could
  // tell that from genuine corruption.
  const server = await startFixtureRelease({
    assets: { "a.tar.gz": BODY },
    faults: { "a.tar.gz": { truncate: true, failTimes: 1 } },
  });
  t.after(() => server.close());
  const root = mkTmpRoot("lr-http-");
  t.after(() => fs.rmSync(root, { recursive: true, force: true }));

  const dest = path.join(root, "a.tar.gz");
  // First attempt truncates; the fault fires once, so the retry gets the whole
  // body. What matters is that the file equals the body exactly.
  const out = await httpGetToFile(`${server.origin}/download/1.0.0/a.tar.gz`, dest, {
    env: CLEAN_ENV,
    backoff: false,
  });
  assert.equal(out.bytesWritten, BODY.length);
  assert.equal(out.sha256, BODY_SHA);
  assert.deepEqual(fs.readFileSync(dest), BODY, "no concatenation of attempts");
});

test("no Authorization header is ever sent, even with a token in the environment", async (t) => {
  const server = await startFixtureRelease({ assets: { "a.tar.gz": BODY } });
  t.after(() => server.close());

  await httpGetToString(`${server.origin}/download/1.0.0/a.tar.gz`, {
    env: { ...CLEAN_ENV, GITHUB_TOKEN: "ghp_secret", GH_TOKEN: "ghp_secret" },
    maxBytes: BODY.length + 1,
  });
  for (const req of server.requests()) {
    assert.equal(req.headers.authorization, undefined, "the repository is public");
  }
});
