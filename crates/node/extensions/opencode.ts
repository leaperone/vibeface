// vibeface: `vibeface hooks install` writes this file and `vibeface hooks uninstall` removes it.
// @ts-nocheck
// The Node's own opencode serve loads a copy whose HOOK is null: there it gives each session's
// tool shells the tool environment of its turn, which the Node keeps in VIBEFACE_OPENCODE_ENV.
import { readFileSync } from "node:fs";
import { join } from "node:path";

const HOOK = __VIBEFACE_HOOK__;
const ENV_DIR = process.env.VIBEFACE_OPENCODE_ENV;

function read(file) {
  try {
    return JSON.parse(readFileSync(file, "utf8"));
  } catch {
    return {};
  }
}

export default {
  id: HOOK ? "vibeface" : "vibeface-serve",
  server: async () => {
    if (HOOK || !ENV_DIR) return {};
    return {
      // The serve's own settings and anything that looks like a key are blanked first.
      "shell.env": async (input, output) => {
        Object.assign(output.env, read(join(ENV_DIR, "default.json")));
        const id = input?.sessionID;
        if (typeof id === "string" && /^ses_[A-Za-z0-9]+$/.test(id)) {
          Object.assign(output.env, read(join(ENV_DIR, `${id}.json`)));
        }
      },
    };
  },
};
