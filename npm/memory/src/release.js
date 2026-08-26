"use strict";

// Names and URLs for the GitHub release assets, and parsers for the two byte
// shapes the release publishes. Pure string work: this module opens no socket
// and touches no filesystem — the socket is `src/http.js`'s (T22-06). Keeping
// the split lets every naming and parsing rule be tested without a server.
//
// ADR-0013 (`docs/adr/0013-binary-delivery-via-release-assets.md`) is why the
// binaries come from a release rather than from per-platform npm packages.

const { targetTriple, exeSuffix } = require("./platform");

const DEFAULT_RELEASE_BASE_URL = "https://github.com/13W/memory/releases";

/**
 * Where the release assets live. `LOCAL_RAG_RELEASE_BASE_URL` overrides it, and
 * that variable is deliberately not named `LOCAL_RAG_TEST_*`: it is both the
 * seam the offline test fixture server uses and the supported way to point an
 * air-gapped or mirrored install somewhere else.
 *
 * @param {NodeJS.ProcessEnv} [env]
 * @returns {string} without a trailing slash
 */
function releaseBaseUrl(env = process.env) {
  const raw = env.LOCAL_RAG_RELEASE_BASE_URL || DEFAULT_RELEASE_BASE_URL;
  return raw.replace(/\/+$/, "");
}

/**
 * The archive `cargo-dist` publishes for one binary on one platform.
 *
 * The extension is `.tar.gz` ahead of the producer: `dist-workspace.toml` still
 * emits `.tar.xz` until T22-07 sets `unix-archive`, so between these two cards
 * this function names an asset that the *current* tag does not carry. That is
 * safe because nothing downloads before T22-08 and T22-17 cuts the new tag, and
 * it is deliberate rather than an oversight — Node has gzip and inflate built in
 * and will never have xz, so the format has to move for the installer to work
 * without shelling out.
 *
 * @param {string} binary e.g. "local-rag-proxy"
 * @param {string} key e.g. "darwin-arm64"
 * @returns {string|null} null when the key has no release asset at all
 */
function assetName(binary, key) {
  const triple = targetTriple(key);
  if (triple === null) {
    return null;
  }
  const ext = key.startsWith("win32-") ? ".zip" : ".tar.gz";
  return `${binary}-${triple}${ext}`;
}

/** @param {string} asset @param {NodeJS.ProcessEnv} [env] @returns {string} */
function latestAssetUrl(asset, env = process.env) {
  return `${releaseBaseUrl(env)}/latest/download/${asset}`;
}

/**
 * @param {string} tag e.g. "0.0.0"
 * @param {string} asset
 * @param {NodeJS.ProcessEnv} [env]
 * @returns {string}
 */
function pinnedAssetUrl(tag, asset, env = process.env) {
  return `${releaseBaseUrl(env)}/download/${tag}/${asset}`;
}

/** @param {string} binary @param {string} asset @returns {string} */
function sidecarName(asset) {
  return `${asset}.sha256`;
}

/**
 * The name the extracted executable must have on disk.
 *
 * @param {string} binary @param {string} [platform] @returns {string}
 */
function executableName(binary, platform = process.platform) {
  return `${binary}${exeSuffix(platform)}`;
}

/**
 * Recover the resolved tag from the `Location` of `/latest/download/<asset>`.
 *
 * GitHub answers that path with a 302 whose `Location` names the concrete tag,
 * so the tag is knowable before any payload moves and without spending a
 * rate-limited API call. Throws rather than returning null: a redirect that does
 * not look like a release download is a broken assumption, not a missing value.
 *
 * @param {string} location @returns {string}
 */
function parseTagFromLocation(location) {
  if (typeof location !== "string" || location.length === 0) {
    throw new Error("local-rag: redirect carried no Location header");
  }
  const withoutQuery = location.split("?")[0].split("#")[0];
  // `/download/<tag>/<asset>` — the shape `pinnedAssetUrl` builds. The
  // `/releases` segment belongs to the *base* URL, not to this shape: on
  // github.com the base already ends with it, and a mirror pointed at by
  // `LOCAL_RAG_RELEASE_BASE_URL` need not be GitHub-shaped at all. Requiring
  // it here would have made the documented mirror and air-gapped paths
  // unusable, which is what `D-109` records.
  const m = /\/download\/([^/]+)\/[^/]+$/.exec(withoutQuery);
  if (m === null || m[1].length === 0) {
    throw new Error(
      `local-rag: redirect Location is not a release download URL: ${location}`,
    );
  }
  return decodeURIComponent(m[1]);
}

const SHA256_RE = /^[0-9a-f]{64}$/;

/**
 * Parse a `.sha256` sidecar and return the digest it certifies for `asset`.
 *
 * The published shape is coreutils binary mode — `<64 hex> *<filename>` — with
 * a trailing blank line. Both the two-space text-mode form and the bare-digest
 * form are accepted, because which one a tool emits is not something this
 * installer should be brittle about; what it must not accept is a digest that
 * belongs to a *different* file, so the name is checked whenever one is present.
 *
 * @param {string} text @param {string} asset @returns {string} lowercase hex
 */
function parseSha256Sidecar(text, asset) {
  if (typeof text !== "string") {
    throw new Error(`local-rag: ${asset}.sha256 was not text`);
  }
  const line = text.split("\n").find((l) => l.trim().length > 0);
  if (line === undefined) {
    throw new Error(`local-rag: ${asset}.sha256 is empty`);
  }
  const parts = line.trim().split(/\s+/);
  const digest = parts[0].toLowerCase();
  if (!SHA256_RE.test(digest)) {
    throw new Error(
      `local-rag: ${asset}.sha256 does not start with a 64-character hex digest (got "${parts[0]}")`,
    );
  }
  if (parts.length > 1) {
    const named = parts[1].replace(/^\*/, "");
    if (named !== asset) {
      throw new Error(
        `local-rag: ${asset}.sha256 certifies a different file ("${named}")`,
      );
    }
  }
  return digest;
}

module.exports = {
  DEFAULT_RELEASE_BASE_URL,
  releaseBaseUrl,
  assetName,
  sidecarName,
  executableName,
  latestAssetUrl,
  pinnedAssetUrl,
  parseTagFromLocation,
  parseSha256Sidecar,
};
