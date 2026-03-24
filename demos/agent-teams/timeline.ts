// Scripted 10-minute scenario for the agent teams demo
import * as fs from "node:fs";
import * as path from "node:path";
import { dataDir, addActivity, reloadAgents, elapsedTime, paused } from "./state.ts";

interface TimelineEvent {
  atSeconds: number;
  action: () => void;
}

function writeStatus(agentPath: string, name: string, status: string, task: string, progress: number, body: string): void {
  const dir = dataDir.peek();
  const fullPath = path.join(dir, agentPath);
  fs.mkdirSync(path.dirname(fullPath), { recursive: true });
  const content = `---
name: ${name}
status: ${status}
task: ${task}
progress: ${progress.toFixed(2)}
started: 2024-03-20T10:00:00Z
updated: ${new Date().toISOString()}
---
${body}
`;
  fs.writeFileSync(fullPath, content);
}

function writePlan(agentPath: string, plan: string): void {
  const dir = dataDir.peek();
  const fullPath = path.join(dir, agentPath);
  fs.mkdirSync(path.dirname(fullPath), { recursive: true });
  fs.writeFileSync(fullPath, plan);
}

function writeOutput(agentPath: string, output: string): void {
  const dir = dataDir.peek();
  const fullPath = path.join(dir, agentPath);
  fs.mkdirSync(path.dirname(fullPath), { recursive: true });
  fs.writeFileSync(fullPath, output);
}

export const timeline: TimelineEvent[] = [
  // --- Phase 1: Researcher wraps up (5s-30s at default speed) ---

  // 0:05 — Researcher progress update
  {
    atSeconds: 5,
    action() {
      writeStatus("main-agent/sub-agents/researcher/status.md", "Researcher", "working", "Analyzing competitor pricing data", 0.75, "Scraping 8/10 competitor websites.\nFound pricing data for 9 products so far.");
      addActivity("Researcher", "Scraped 8/10 competitor sites");
    },
  },
  // 0:15 — Researcher progress
  {
    atSeconds: 15,
    action() {
      writeStatus("main-agent/sub-agents/researcher/status.md", "Researcher", "working", "Analyzing competitor pricing data", 0.9, "Scraping 9/10 competitor websites.\nCompiling final dataset.");
    },
  },
  // 0:25 — Researcher sends data to Coder
  {
    atSeconds: 25,
    action() {
      addActivity("Researcher \u2192 Coder", "Here's the pricing data you requested \u2014 9 competitors analyzed so far");
    },
  },
  // 0:30 — Researcher completes
  {
    atSeconds: 30,
    action() {
      writeStatus("main-agent/sub-agents/researcher/status.md", "Researcher", "done", "Research complete", 1.0, "Completed scraping all 10 competitor websites.\nFull pricing data collected for 12 products.");
      writeOutput("main-agent/sub-agents/researcher/output.md", "# Research Output\n\nAnalyzed 10 competitors, 12 products.\nPricing range: $29-$199/mo.\nKey findings:\n- 70% offer free tier\n- Average enterprise price: $149/mo\n- Most charge per-seat\n");
      addActivity("Researcher", "Completed research \u2014 10 competitors analyzed");
      addActivity("Researcher \u2192 Main Agent", "Research complete. Full dataset ready for integration.");
    },
  },

  // --- Phase 2: Test Writer finds bug, Coder blocked (45s-90s) ---

  // 0:45 — Coder progress
  {
    atSeconds: 45,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "working", "Implementing pricing analysis tool", 0.5, "Core data models done. Building API layer.\nIntegrating research data.");
      writeStatus("main-agent/sub-agents/coder/sub-agents/test-writer/status.md", "Test Writer", "working", "Writing unit tests for data models", 0.5, "Writing unit tests for core models.\n22 tests written so far.");
      addActivity("Coder", "Integrating research data into models");
      addActivity("Test Writer \u2192 Coder", "Tests for data models look good so far, continuing with API tests");
    },
  },
  // 1:00 — Test Writer finds a bug
  {
    atSeconds: 60,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/test-writer/status.md", "Test Writer", "error", "Found failing test", 0.6, "FAIL: test_price_comparison \u2014 null pointer on missing competitor data.\nThe pricing model doesn't handle missing entries.");
      addActivity("Test Writer", "Found bug: null pointer in price comparison");
      addActivity("Test Writer \u2192 Coder", "BLOCKER: test_price_comparison fails with null pointer when competitor data is missing");
    },
  },
  // 1:10 — Coder blocked
  {
    atSeconds: 70,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "blocked", "Bug found by Test Writer", 0.5, "Blocked: null pointer in price comparison.\nNeed to add null checks for missing competitor data.");
      addActivity("Coder", "Blocked on null pointer bug");
      addActivity("Coder \u2192 Test Writer", "Acknowledged. Working on fix for missing data handling.");
    },
  },
  // 1:30 — Coder fixes bug, resumes
  {
    atSeconds: 90,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "working", "Implementing pricing analysis tool", 0.6, "Fixed null pointer bug. Added optional chaining for missing competitor data.\nResuming API implementation.");
      writeStatus("main-agent/sub-agents/coder/sub-agents/test-writer/status.md", "Test Writer", "working", "Running test suite", 0.7, "All tests passing now. Writing integration tests.\n30 tests, all green.");
      addActivity("Coder", "Fixed null pointer bug, resumed implementation");
      addActivity("Coder \u2192 Test Writer", "Fix pushed. Please re-run the full suite.");
      addActivity("Test Writer", "All tests passing after fix");
    },
  },

  // --- Phase 3: Designer completes, Reviewer starts (120s-180s) ---

  // 2:00 — Designer progress
  {
    atSeconds: 120,
    action() {
      writeStatus("main-agent/sub-agents/designer/status.md", "Designer", "working", "Creating dashboard mockups", 0.7, "Dashboard layout finalized.\nWorking on chart components for pricing comparison.");
      addActivity("Designer", "Dashboard layout finalized, building charts");
    },
  },
  // 2:15 — Designer sends update to Main Agent
  {
    atSeconds: 135,
    action() {
      addActivity("Designer \u2192 Main Agent", "Dashboard mockups 70% done \u2014 pricing charts looking great");
    },
  },
  // 2:30 — Reviewer starts reviewing
  {
    atSeconds: 150,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/reviewer/status.md", "Reviewer", "working", "Reviewing code quality", 0.2, "Reviewing data model layer.\nChecking for SOLID principles compliance.");
      addActivity("Reviewer", "Started code review");
      addActivity("Reviewer \u2192 Coder", "Starting review of data model and API layers");
    },
  },
  // 2:45 — Test Writer completes
  {
    atSeconds: 165,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/test-writer/status.md", "Test Writer", "done", "All tests passing", 1.0, "42 unit tests, 8 integration tests. All green.");
      writeOutput("main-agent/sub-agents/coder/sub-agents/test-writer/output.md", "# Test Results\n\n42 unit tests, 8 integration tests.\nAll passing. Coverage: 87%.\n");
      addActivity("Test Writer", "Completed \u2014 42 unit tests, 8 integration tests");
      addActivity("Test Writer \u2192 Coder", "Full test suite green. 87% coverage.");
    },
  },
  // 3:00 — Designer completes
  {
    atSeconds: 180,
    action() {
      writeStatus("main-agent/sub-agents/designer/status.md", "Designer", "done", "Dashboard mockups complete", 1.0, "All dashboard mockups complete.\n3 pages: Overview, Comparison, Detail.\n12 chart components designed.");
      writeOutput("main-agent/sub-agents/designer/output.md", "# Design Output\n\n3 dashboard pages designed:\n- Overview (summary metrics)\n- Comparison (side-by-side pricing)\n- Detail (per-competitor breakdown)\n\n12 chart components, responsive layout.\n");
      addActivity("Designer", "Completed dashboard mockups \u2014 3 pages, 12 components");
      addActivity("Designer \u2192 Main Agent", "All mockups delivered. Ready for implementation.");
    },
  },

  // --- Phase 4: Reviewer finds issues, Coder blocked again (210s-270s) ---

  // 3:30 — Reviewer finds issues
  {
    atSeconds: 210,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/reviewer/status.md", "Reviewer", "working", "Found issues", 0.6, "Issues found:\n1. Magic numbers in pricing calculation\n2. Missing error handling in API calls\n3. No retry logic for failed scrapes");
      addActivity("Reviewer", "Found 3 issues in code review");
      addActivity("Reviewer \u2192 Coder", "3 issues found: magic numbers, missing error handling, no retry logic. See review comments.");
    },
  },
  // 3:40 — Coder blocked on review
  {
    atSeconds: 220,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "blocked", "Reviewer found issues", 0.75, "Addressing code review feedback:\n- Extract constants\n- Add error handling\n- Implement retry logic");
      addActivity("Coder", "Blocked on review feedback");
      addActivity("Coder \u2192 Reviewer", "Acknowledged all 3 issues. Working on fixes now.");
    },
  },
  // 4:00 — Coder progress on fixing issues
  {
    atSeconds: 240,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "working", "Fixing review issues", 0.8, "Fixed magic numbers and error handling.\nImplementing retry logic with exponential backoff.");
      addActivity("Coder", "Fixed 2/3 review issues");
    },
  },
  // 4:30 — Coder fixes all issues, Reviewer approves
  {
    atSeconds: 270,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "working", "Finalizing implementation", 0.9, "All review feedback addressed. Final cleanup.");
      writeStatus("main-agent/sub-agents/coder/sub-agents/reviewer/status.md", "Reviewer", "done", "Review approved", 1.0, "All issues resolved. Code quality acceptable.\nApproved for merge.");
      addActivity("Coder", "Fixed all review issues");
      addActivity("Reviewer", "Approved \u2014 all issues resolved");
      addActivity("Reviewer \u2192 Coder", "LGTM! All issues addressed. Approved for merge.");
    },
  },

  // --- Phase 5: Coder completes, Deployer spawns (300s-390s) ---

  // 5:00 — Coder done
  {
    atSeconds: 300,
    action() {
      writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "done", "Implementation complete", 1.0, "Pricing analysis tool fully implemented and reviewed.\nAll tests passing, all review issues addressed.");
      writeOutput("main-agent/sub-agents/coder/output.md", "# Implementation Output\n\nBuilt pricing analysis tool:\n- Data models for 12 product categories\n- REST API with 6 endpoints\n- Report generator with PDF/CSV export\n- 42 unit tests + 8 integration tests\n- All code reviewed and approved\n");
      addActivity("Coder", "Completed implementation");
      addActivity("Coder \u2192 Main Agent", "Implementation complete. All tests green, review approved.");
    },
  },
  // 5:15 — Main Agent spawns Deployer as Coder sub-agent
  {
    atSeconds: 315,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/deployer/status.md", "Deployer", "working", "Preparing deployment pipeline", 0.1, "Setting up CI/CD pipeline.\nConfiguring staging environment.");
      writeStatus("main-agent/status.md", "Main Agent", "working", "Coordinating full-stack feature implementation", 0.7, "Research complete. Implementation complete.\nDesign complete. Preparing deployment.");
      addActivity("Main Agent", "Spawned Deployer for deployment prep");
      addActivity("Main Agent \u2192 Deployer", "Please set up CI/CD and deploy to staging");
    },
  },
  // 5:45 — Deployer progress
  {
    atSeconds: 345,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/deployer/status.md", "Deployer", "working", "Preparing deployment pipeline", 0.4, "CI/CD pipeline configured.\nRunning deployment checks.\nStaging environment ready.");
      addActivity("Deployer", "CI/CD pipeline configured, running checks");
      addActivity("Deployer \u2192 Main Agent", "Staging environment ready. Running deployment checks.");
    },
  },
  // 6:00 — Deployer progress
  {
    atSeconds: 360,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/deployer/status.md", "Deployer", "working", "Preparing deployment pipeline", 0.6, "All checks passing.\nDeploying to staging environment.\nRunning smoke tests.");
      addActivity("Deployer", "Deploying to staging environment");
    },
  },
  // 6:30 — Deployer completes
  {
    atSeconds: 390,
    action() {
      writeStatus("main-agent/sub-agents/coder/sub-agents/deployer/status.md", "Deployer", "done", "Deployment complete", 1.0, "Successfully deployed to staging.\nAll smoke tests passing.\nReady for production deployment.");
      writeOutput("main-agent/sub-agents/coder/sub-agents/deployer/output.md", "# Deployment Output\n\nDeployed to staging:\n- CI/CD pipeline: configured\n- Staging URL: https://staging.example.com\n- Smoke tests: 12/12 passing\n- Ready for production\n");
      addActivity("Deployer", "Deployment complete \u2014 staging live, all smoke tests passing");
      addActivity("Deployer \u2192 Main Agent", "Staging deployment successful. Ready for production when you are.");
    },
  },

  // --- Phase 6: Final compilation and completion (450s-600s) ---

  // 7:30 — Main Agent starts compiling
  {
    atSeconds: 450,
    action() {
      writeStatus("main-agent/status.md", "Main Agent", "working", "Compiling final results", 0.85, "Integrating research findings with implementation.\nMerging design assets.\nGenerating final report.");
      addActivity("Main Agent", "Compiling final results");
      addActivity("Main Agent \u2192 Researcher", "Using your research data for the final report");
      addActivity("Main Agent \u2192 Designer", "Integrating your mockups into the deliverables");
    },
  },
  // 8:00 — Main Agent progress
  {
    atSeconds: 480,
    action() {
      writeStatus("main-agent/status.md", "Main Agent", "working", "Compiling final results", 0.92, "Final report draft complete.\nReviewing all deliverables.");
      addActivity("Main Agent", "Final report draft complete");
    },
  },
  // 9:00 — Main Agent nearing completion
  {
    atSeconds: 540,
    action() {
      writeStatus("main-agent/status.md", "Main Agent", "working", "Final review", 0.97, "All deliverables verified.\nRunning final quality checks.");
      addActivity("Main Agent", "Running final quality checks");
    },
  },
  // 10:00 — All done
  {
    atSeconds: 600,
    action() {
      writeStatus("main-agent/status.md", "Main Agent", "done", "Project complete", 1.0, "All tasks completed successfully.\nResearch: 10 competitors analyzed\nTool: fully implemented and tested\nDashboard: designed and ready\nDeployment: staging live\nReport: generated and ready for review");
      writeOutput("main-agent/output.md", "# Final Report\n\nCompetitor pricing analysis complete.\n\n## Key Findings\n- 10 competitors analyzed\n- Price range: $29-$199/mo\n- Our recommended pricing: $79/mo (mid-market)\n\n## Deliverables\n1. Research data (12 products, 10 competitors)\n2. Pricing analysis tool (6 API endpoints)\n3. Dashboard mockups (3 pages, 12 components)\n4. Deployment (staging live, smoke tests passing)\n5. Report generator (PDF/CSV)\n\nAll agents completed successfully.\n");
      addActivity("Main Agent", "Project complete!");
    },
  },
];

// --- Timeline runner ---
let tickTimer: ReturnType<typeof setInterval> | null = null;
let nextEventIdx = 0;

export function startTimeline(): void {
  const speed = parseInt(process.env.SPEED ?? "1", 10) || 1;
  const tickMs = Math.max(50, Math.floor(1000 / speed));

  nextEventIdx = 0;

  tickTimer = setInterval(() => {
    if (paused.peek()) return;

    const elapsed = elapsedTime.peek() + 1;
    elapsedTime.set(elapsed);

    // Fire events that should have happened by now
    while (nextEventIdx < timeline.length && timeline[nextEventIdx].atSeconds <= elapsed) {
      timeline[nextEventIdx].action();
      nextEventIdx++;
    }

    // Reload agent tree from disk (the actions write files, watcher picks up changes too)
    reloadAgents();

    // Stop when all events are done and 5 seconds have passed since last event
    if (nextEventIdx >= timeline.length && elapsed > timeline[timeline.length - 1].atSeconds + 5) {
      // Keep running but stop advancing
    }
  }, tickMs);
}

export function stopTimeline(): void {
  if (tickTimer) {
    clearInterval(tickTimer);
    tickTimer = null;
  }
}

// --- Initialize data directory with rich initial state ---
export function initDataDir(): void {
  const dir = dataDir.peek();

  // Create all agent directories and status files for initial state
  // Main Agent (working)
  writeStatus("main-agent/status.md", "Main Agent", "working", "Coordinating full-stack feature implementation", 0.15, "Project in progress. Research, coding, and design underway.");
  writePlan("main-agent/plan.md", "# Plan\n\n1. Research competitor pricing\n2. Implement scraping tool\n3. Design dashboard\n4. Analyze data\n5. Deploy to staging\n6. Generate report\n");

  // Researcher (working, 0.65)
  writeStatus("main-agent/sub-agents/researcher/status.md", "Researcher", "working", "Analyzing competitor pricing data", 0.65, "Scraping 7/10 competitor websites.");

  // Coder (working, 0.4)
  writeStatus("main-agent/sub-agents/coder/status.md", "Coder", "working", "Implementing pricing analysis tool", 0.4, "Core data models done. Building API layer.");

  // Test Writer (working, 0.3) — sub-agent of Coder
  writeStatus("main-agent/sub-agents/coder/sub-agents/test-writer/status.md", "Test Writer", "working", "Writing unit tests for data models", 0.3, "Writing unit tests for core models.");

  // Reviewer (idle) — sub-agent of Coder
  writeStatus("main-agent/sub-agents/coder/sub-agents/reviewer/status.md", "Reviewer", "idle", "Waiting for code review assignment", 0, "Initialized. Waiting for code to review.");

  // Designer (working, 0.5)
  writeStatus("main-agent/sub-agents/designer/status.md", "Designer", "working", "Creating dashboard mockups", 0.5, "Working on dashboard layout and chart components.");

  // Icon Artist (done, 1.0) — sub-agent of Designer
  writeStatus("main-agent/sub-agents/designer/sub-agents/icon-artist/status.md", "Icon Artist", "done", "Completed icon set", 1.0, "Delivered 24 custom icons for the dashboard.");
  writeOutput("main-agent/sub-agents/designer/sub-agents/icon-artist/output.md", "# Icon Set\n\n24 custom icons delivered:\n- 8 navigation icons\n- 8 chart icons\n- 8 action icons\n\nAll in SVG format, 24x24 grid.\n");

  // Seed activity log with staggered timestamps so they look natural
  addActivity("Main Agent", "Started project coordination", "10:00");
  addActivity("Researcher", "Began competitor analysis", "10:15");
  addActivity("Coder", "Started implementation", "10:20");
  addActivity("Designer", "Started dashboard mockups", "10:25");
  addActivity("Icon Artist", "Completed icon set \u2014 24 icons delivered", "10:30");
  addActivity("Test Writer", "Started writing tests", "10:32");

  // Set nextEventIdx to 0 so timeline events start from the beginning
  nextEventIdx = 0;

  reloadAgents();
}
