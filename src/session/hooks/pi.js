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
}
