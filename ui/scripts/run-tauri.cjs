const path = require("node:path");

// Tauri CLI searches downward from cwd for `src-tauri/tauri.conf.json`,
// so normalize cwd to the workspace root when invoked from `ui/`.
process.chdir(path.resolve(__dirname, "..", ".."));
require("@tauri-apps/cli/tauri.js");
