#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { pathToFileURL } from "node:url";

function globalNpmRoot() {
  return execFileSync("npm", ["root", "-g"], { encoding: "utf8" }).trim();
}

const packageRoot = `${process.env.THREADMILL_CLAUDE_AGENT_ACP_ROOT ?? `${globalNpmRoot()}/@agentclientprotocol/claude-agent-acp`}`;

async function importFromPackage(relativePath) {
  return import(pathToFileURL(`${packageRoot}/${relativePath}`).href);
}

const { ClaudeAcpAgent } = await importFromPackage("dist/acp-agent.js");
const acpSdk = await importFromPackage("node_modules/@agentclientprotocol/sdk/dist/acp.js");
const { AgentSideConnection } = acpSdk;
const createJsonStream = acpSdk["ndJsonStream"];
const { nodeToWebReadable, nodeToWebWritable } = await importFromPackage("dist/utils.js");

const EFFORT_CONFIG_ID = "effort";
const EFFORT_OPTIONS = [
  { value: "low", name: "Low", description: "Minimal thinking, fastest responses" },
  { value: "medium", name: "Medium", description: "Moderate thinking" },
  { value: "high", name: "High", description: "Deep reasoning" },
  { value: "max", name: "Max", description: "Maximum effort" },
];

function currentEffortForSession(session) {
  return session.threadmillEffort ?? session.settingsManager?.getSettings?.().effortLevel ?? "high";
}

function effortConfigOption(session) {
  return {
    id: EFFORT_CONFIG_ID,
    name: "Effort",
    description: "Claude reasoning effort",
    category: "effort",
    type: "select",
    currentValue: currentEffortForSession(session),
    options: EFFORT_OPTIONS,
  };
}

function withEffortConfigOptions(configOptions, session) {
  const options = Array.isArray(configOptions) ? configOptions.filter((option) => option.id !== EFFORT_CONFIG_ID) : [];
  return [...options, effortConfigOption(session)];
}

function syncEffortConfig(session, effort) {
  session.threadmillEffort = effort;
  session.configOptions = withEffortConfigOptions(session.configOptions, session);
}

class ThreadmillClaudeAcpAgent extends ClaudeAcpAgent {
  attachEffortOption(response) {
    const session = this.sessions?.[response?.sessionId];
    if (!session) return response;
    syncEffortConfig(session, currentEffortForSession(session));
    return { ...response, configOptions: session.configOptions };
  }

  async newSession(params) {
    return this.attachEffortOption(await super.newSession(params));
  }

  async loadSession(params) {
    return this.attachEffortOption(await super.loadSession(params));
  }

  async unstable_resumeSession(params) {
    return this.attachEffortOption(await super.unstable_resumeSession(params));
  }

  async unstable_forkSession(params) {
    return this.attachEffortOption(await super.unstable_forkSession(params));
  }

  async setSessionConfigOption(params) {
    if (params.configId !== EFFORT_CONFIG_ID) {
      return super.setSessionConfigOption(params);
    }

    const session = this.sessions?.[params.sessionId];
    if (!session) {
      throw new Error("Session not found");
    }
    if (typeof params.value !== "string") {
      throw new Error(`Invalid value for config option ${params.configId}: ${params.value}`);
    }
    if (!EFFORT_OPTIONS.some((option) => option.value === params.value)) {
      throw new Error(`Invalid value for config option ${params.configId}: ${params.value}`);
    }

    await session.query.applyFlagSettings({ effortLevel: params.value });
    syncEffortConfig(session, params.value);
    await this.client.sessionUpdate({
      sessionId: params.sessionId,
      update: {
        sessionUpdate: "config_option_update",
        configOptions: session.configOptions,
      },
    });
    return { configOptions: session.configOptions };
  }
}

const input = nodeToWebWritable(process.stdout);
const output = nodeToWebReadable(process.stdin);
const stream = createJsonStream(input, output);
new AgentSideConnection((client) => new ThreadmillClaudeAcpAgent(client), stream);
