// Managed by radar setup. Observe the visible root session, not server activity
// (background/subagent requests do not identify the conversation in this pane).
import { execFileSync } from "node:child_process";
import { createEffect } from "solid-js";

export default {
  id: "radar-session",
  setup(ctx) {
    if (!process.env.RADAR_AGENT) return;
    let reported;
    return ctx.ui.slot({
      append: "app",
      render: () => {
        createEffect(() => {
          const route = ctx.ui.router.current();
          if (route.type !== "session" || route.sessionID === reported) return;
          const session = ctx.data.session.get(route.sessionID);
          if (!session || session.parentID) return;
          execFileSync("radar", ["session", "identify", "--provider", "opencode",
            "--conversation", route.sessionID, "--pid", String(process.pid)], {
            timeout: 10000, stdio: ["ignore", "pipe", "pipe"],
          });
          process.env.RADAR_PROVIDER_SESSION_ID = route.sessionID;
          reported = route.sessionID;
        });
        return null;
      },
    });
  },
};
