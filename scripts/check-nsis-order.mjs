import { readFileSync } from "node:fs";

const path = process.argv[2];
if (!path) {
  throw new Error("Usage: node scripts/check-nsis-order.mjs INSTALLER_NSI");
}

const source = readFileSync(path, "utf8");
const pageLeave = source.indexOf("Function PageLeaveReinstall");
const uninstall = source.indexOf("ExecWait '$R1'", pageLeave);
const install = source.indexOf("Section Install");
const preinstall = source.indexOf("!insertmacro NSIS_HOOK_PREINSTALL", install);
const copy = source.indexOf('File "${MAINBINARYSRCPATH}"', install);

for (const [name, offset] of Object.entries({
  pageLeave,
  uninstall,
  install,
  preinstall,
  copy,
})) {
  if (offset < 0) throw new Error(`Generated NSIS script has no ${name}`);
}
if (!(pageLeave < uninstall && uninstall < install && install < preinstall && preinstall < copy)) {
  throw new Error("Unexpected NSIS reinstall/install order; inspect generated script");
}

const line = (offset) => source.slice(0, offset).split("\n").length;
console.log(`Tauri NSIS script: ${path}`);
console.log(`Previous-uninstall ExecWait: line ${line(uninstall)}`);
console.log(`PREINSTALL: line ${line(preinstall)}`);
console.log(`Main executable copy: line ${line(copy)}`);
console.log("The stock PREINSTALL hook is too late when reinstall chooses uninstall.");
