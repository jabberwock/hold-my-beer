import * as vscode from "vscode";
import { CollabApi } from "./api";
import { getCollabConfig } from "./config";
import { registerCollabCommands, openChatPanel } from "./commands";
import { CollabChatPanel } from "./chat";
import { CollabSse } from "./sse";
import { notifyIncomingMessage } from "./notifications";
import { RosterProvider } from "./roster";

export function activate(context: vscode.ExtensionContext): void {
  const outputChannel = vscode.window.createOutputChannel("Collab");
  context.subscriptions.push(outputChannel);

  let cfg = getCollabConfig();
  let api = new CollabApi(cfg);

  const getApi = () => api;
  const roster = new RosterProvider(getApi);

  const treeView = vscode.window.createTreeView("collab.roster", {
    treeDataProvider: roster,
    showCollapseAll: false,
  });
  context.subscriptions.push(treeView);

  let sse: CollabSse | undefined;
  let tick: ReturnType<typeof setInterval> | undefined;

  const statusBar = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left,
    100
  );
  statusBar.command = "collab.openChat";
  context.subscriptions.push(statusBar);

  function updateStatus(): void {
    if (!cfg.instance) {
      statusBar.text = "$(comment-discussion) collab: (not configured)";
      statusBar.tooltip = "Set collab.instance (and collab.token if required)";
      statusBar.show();
      return;
    }
    const n = roster.countOnline();
    statusBar.text = `$(comment-discussion) collab: @${cfg.instance} (${n} online)`;
    statusBar.tooltip = "Open Collab chat";
    statusBar.show();
  }

  function openChat(prefill?: string): void {
    openChatPanel(context.extensionUri, getApi, () => cfg.instance, prefill);
  }

  function startSse(): void {
    sse?.dispose();
    sse = undefined;
    if (!cfg.instance || !cfg.token) {
      return;
    }
    sse = new CollabSse(cfg.server, cfg.token, cfg.instance, (msg) => {
      notifyIncomingMessage(msg, cfg.instance, () => {
        openChat();
      });
      CollabChatPanel.current?.appendMessage(msg);
    });
    sse.start();
  }

  async function heartbeatAndRefresh(): Promise<void> {
    if (!cfg.instance) {
      updateStatus();
      return;
    }
    if (!cfg.token) {
      updateStatus();
      return;
    }
    const role = vscode.workspace.name
      ? `VS Code (${vscode.workspace.name})`
      : "VS Code";
    try {
      await api.putPresence(cfg.instance, role);
    } catch {
      // network error — still try roster
    }
    try {
      await roster.loadFromServer();
    } catch {
      // ignore
    }
    updateStatus();
  }

  registerCollabCommands(context, {
    getApi,
    getConfig: () => cfg,
    outputChannel,
    onOpenChat: openChat,
  });

  void roster
    .loadFromServer()
    .then(() => updateStatus())
    .catch(() => updateStatus());
  void heartbeatAndRefresh();
  startSse();
  tick = setInterval(() => {
    void heartbeatAndRefresh();
  }, 30_000);

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("collab")) {
        cfg = getCollabConfig();
        api = new CollabApi(cfg);
        updateStatus();
        startSse();
        void heartbeatAndRefresh();
      }
    })
  );

  context.subscriptions.push({
    dispose: () => {
      if (tick !== undefined) {
        clearInterval(tick);
      }
      sse?.dispose();
      const last = getCollabConfig();
      if (last.instance && last.token) {
        void new CollabApi(last).deletePresence(last.instance);
      }
    },
  });
}

export function deactivate(): void {}
