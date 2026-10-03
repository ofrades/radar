// Managed by radar setup. Only the root worker may report its active session.
import { execFileSync } from "node:child_process";

export default function (pi) {
  if (!process.env.RADAR_AGENT || process.env.PI_SUBAGENT_CHILD) return;
  let reported;
  function identify(_event, ctx) {
    const id = ctx.sessionManager.getSessionId();
    if (id === reported) return;
    execFileSync("radar", ["session", "identify", "--provider", process.env.RADAR_SESSION_PROVIDER,
      "--conversation", id, "--pid", String(process.pid)], {
      timeout: 10000, stdio: ["ignore", "pipe", "pipe"],
    });
    process.env.RADAR_PROVIDER_SESSION_ID = id;
    reported = id;
  }
  pi.on("session_start", identify);
  pi.on("before_agent_start", identify);
  // Root sessions report turn boundaries so the driver can hand cards back
  // while the worker is still alive, not only when the process exits.
  if (process.env.RADAR_SESSION_ID) {
    pi.on("agent_end", () => {
      try {
        execFileSync("radar", ["session", "turn-ended"], {
          timeout: 5000, stdio: ["ignore", "ignore", "pipe"],
        });
      } catch {
        // The connector is advisory: a busy or absent daemon is not a turn failure.
      }
    });
  }
}
