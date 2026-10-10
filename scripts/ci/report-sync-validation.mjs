import fs from "node:fs";

const outcomes = JSON.parse(process.env.CHECK_OUTCOMES ?? "{}");
const failed = Object.entries(outcomes).filter(
  ([, result]) => result !== "success",
);
const escape = (value) =>
  String(value)
    .replaceAll("%", "%25")
    .replaceAll("\r", "%0D")
    .replaceAll("\n", "%0A");
const summary = Object.entries(outcomes)
  .map(([name, result]) => `| ${name} | ${result} |`)
  .join("\n");
if (process.env.GITHUB_STEP_SUMMARY) {
  fs.appendFileSync(
    process.env.GITHUB_STEP_SUMMARY,
    `| Check | Result |\n| --- | --- |\n${summary}\n`,
  );
}
for (const [name, outcome] of failed) {
  const file = `validation-logs/${name}.log`;
  const log = fs.existsSync(file)
    ? fs.readFileSync(file, "utf8").replace(/\u001b\[[0-9;]*m/g, "")
    : "No log available";
  const lines = log.split(/\r?\n/);
  const diagnostics = lines
    .filter((line) =>
      /error\[|error TS|^error:|FAIL |AssertionError|Error:|Test Files|Tests .*failed|Formatting issues|\[warn\]/.test(
        line,
      ),
    )
    .slice(0, 30);
  console.log(
    `::error title=${escape(name)}::${escape(`${outcome}\n${(diagnostics.length ? diagnostics : lines.slice(-15)).join("\n")}`)}`,
  );
}
if (failed.length) process.exitCode = 1;
