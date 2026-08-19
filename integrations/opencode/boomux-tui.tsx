import type {
  TuiPlugin,
  TuiPluginModule,
} from "@opencode-ai/plugin/tui";
import { createProcessRunner } from "./boomux-tui-runner.js";
import { createClaimController } from "./boomux-tui-core.js";

const tui: TuiPlugin = async (api) => {
  const controller = createClaimController({
    client: api.client,
    env: globalThis.process?.env ?? {},
    run: createProcessRunner(),
    onError(error: Error & { code?: string }) {
      if (error.code !== "busy") return;
      api.ui.toast({
        variant: "warning",
        title: "Session already active",
        message: "This OpenCode Session belongs to another Boomux ShellRun.",
      });
      api.route.navigate("home");
    },
  });
  if (!controller) return;

  let selected: string | undefined;
  const observe = () => {
    const route = api.route.current;
    const sessionID =
      route.name === "session" ? route.params.sessionID : undefined;
    if (sessionID === selected) return;
    selected = sessionID;
    void controller.select(sessionID);
  };
  observe();
  const timer = setInterval(observe, 250);
  api.lifecycle.onDispose(() => {
    clearInterval(timer);
    return controller.dispose();
  });
};

const plugin: TuiPluginModule & { id: string } = {
  id: "boomux-claim",
  tui,
};

export default plugin;
