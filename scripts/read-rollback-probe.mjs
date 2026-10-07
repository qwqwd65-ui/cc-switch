// Public, read-only Actions diagnostics. No token or credential helper access.
const api = "https://api.github.com/repos/qwqwd65-ui/cc-switch";
const requestedRun = process.argv[2];
const protocol = requestedRun === "--protocol";
if (requestedRun && !protocol && !/^\d+$/.test(requestedRun)) {
  throw new Error("Usage: node scripts/read-rollback-probe.mjs [RUN_ID|--protocol]");
}
async function get(path) {
  const response = await fetch(`${api}/${path}`, {
    headers: { Accept: "application/vnd.github+json" },
    signal: AbortSignal.timeout(20000),
  });
  if (!response.ok) throw new Error(`Public GitHub API HTTP ${response.status}`);
  return response.json();
}
const run = requestedRun && !protocol
  ? await get(`actions/runs/${requestedRun}`)
  : (await get(`actions/workflows/${protocol ? "rollback-protocol" : "windows-rollback-probe"}.yml/runs?per_page=1`))
      .workflow_runs[0];
if (!run) throw new Error("No installer probe run found");
const jobs = (await get(`actions/runs/${run.id}/jobs`)).jobs;
const diagnostics = await Promise.all(jobs.map(async (job) => {
  const annotations = job.status === "completed"
    ? await get(`check-runs/${job.id}/annotations?per_page=100`)
    : [];
  return {
    id: job.id,
    status: job.status,
    conclusion: job.conclusion,
    steps: job.steps.filter(step =>
      step.status === "in_progress" || step.conclusion === "failure"
    ).map(step => ({ name: step.name, status: step.status, conclusion: step.conclusion })),
    annotations: annotations.filter(item => item.annotation_level !== "warning")
      .map(item => ({ title: item.title, message: item.message })),
  };
}));
console.log(JSON.stringify({
  id: run.id, sha: run.head_sha, status: run.status,
  conclusion: run.conclusion, url: run.html_url, jobs: diagnostics,
}, null, 2));
