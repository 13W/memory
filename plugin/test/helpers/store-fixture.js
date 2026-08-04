"use strict";

// Minimal on-disk bootstrap for a fresh `LOCAL_RAG_HOME` so
// `local-rag-hook spool-write` can accept a real write for a given
// session. Full store initialization (`local-rag init`/`StoreLayout::
// ensure`) is out of scope here — this only creates the one directory the
// spool writer itself needs (`<home>/local-rag/spool/<session_id>/`),
// mirroring what the writer's own error message names when it's missing.

const fs = require("node:fs");
const path = require("node:path");

/**
 * @param {string} localRagHome
 * @param {string} sessionId
 * @returns {string} the created spool directory.
 */
function prepareSpoolDir(localRagHome, sessionId) {
  const dir = path.join(localRagHome, "local-rag", "spool", sessionId);
  fs.mkdirSync(dir, { recursive: true });
  return dir;
}

module.exports = { prepareSpoolDir };
