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
const testsLog = "validation-logs/tests.log";
if (fs.existsSync(testsLog)) {
  const totals = fs
    .readFileSync(testsLog, "utf8")
    .replace(/\u001b\[[0-9;]*m/g, "")
    .split(/\r?\n/)
    .filter((line) =>
      /^\s*(Test Files\s|Tests\s+.*(?:passed|failed)|Summary\s+\[)/.test(line),
    );
  if (totals.length) {
    if (process.env.GITHUB_STEP_SUMMARY) {
      fs.appendFileSync(
        process.env.GITHUB_STEP_SUMMARY,
        `\n\`\`\`text\n${totals.join("\n")}\n\`\`\`\n`,
      );
    }
    console.log(
      `::notice title=Regression totals::${escape(totals.join("\n"))}`,
    );
  }
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
