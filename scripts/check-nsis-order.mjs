import { readFileSync } from "node:fs";

const path = process.argv[2];
if (!path) {
  throw new Error("Usage: node scripts/check-nsis-order.mjs INSTALLER_NSI");
}

const source = readFileSync(path, "utf8");
const reinstallPage = source.indexOf("Function PageReinstall");
const reinstallEnd = source.indexOf("FunctionEnd", reinstallPage);
const upgradeGuard = source.indexOf("${If} $WixMode <> 1", reinstallPage);
const upgradeSkip = source.indexOf("    Abort", upgradeGuard);
const pageLeave = source.indexOf("Function PageLeaveReinstall");
const uninstall = source.indexOf("ExecWait '$R1'", pageLeave);
const requireStopped = source.indexOf("Function RequireAppStoppedForRollback");
const requireCall = source.indexOf("Call RequireAppStoppedForRollback");
const install = source.search(/^Section Install\r?$/m);
const preinstall = source.indexOf("!insertmacro NSIS_HOOK_PREINSTALL", install);
const copy = source.indexOf('File "${MAINBINARYSRCPATH}"', install);

for (const [name, offset] of Object.entries({
  reinstallPage,
  reinstallEnd,
  upgradeGuard,
  upgradeSkip,
  pageLeave,
  uninstall,
  requireStopped,
  requireCall,
  install,
  preinstall,
  copy,
})) {
  if (offset < 0) throw new Error(`Generated NSIS script has no ${name}`);
}
if (
  !(
    reinstallPage < upgradeGuard &&
    upgradeGuard < upgradeSkip &&
    upgradeSkip < reinstallEnd &&
    pageLeave < uninstall &&
    uninstall < install &&
    requireStopped < install &&
    install < requireCall &&
    requireCall < preinstall &&
    preinstall < copy
  )
) {
  throw new Error("Unexpected NSIS reinstall/install order; inspect generated script");
}

const stoppedFunction = source.slice(requireStopped, source.indexOf("FunctionEnd", requireStopped));
if (
  !stoppedFunction.includes("FindProcessCurrentUser") ||
  stoppedFunction.includes("KillProcess")
) {
  throw new Error("Rollback installer must require a clean app exit before capture");
}

const line = (offset) => source.slice(0, offset).split("\n").length;
console.log(`Tauri NSIS script: ${path}`);
console.log(`NSIS upgrade skips uninstall page: line ${line(upgradeGuard)}`);
console.log(`Clean-exit check: line ${line(requireCall)}`);
console.log(`PREINSTALL: line ${line(preinstall)}`);
console.log(`Main executable copy: line ${line(copy)}`);
console.log("Upgrade path reaches PREINSTALL before executable replacement.");
