import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";
import type { CollabApi } from "./api";
import type { CollabResolvedConfig } from "./config";
import { CollabChatPanel } from "./chat";
export interface CommandDeps {
  getApi: () => CollabApi;
  getConfig: () => CollabResolvedConfig;
  outputChannel: vscode.OutputChannel;
  onOpenChat: (prefill?: string) => void;
}

function parseSendInput(text: string): { recipient: string; content: string } | null {
  const trimmed = text.trim();
  if (!trimmed) {
    return null;
  }
  const dm = /^@([\w-]+)\s+(.+)$/s.exec(trimmed);
  if (dm) {
    return { recipient: dm[1], content: dm[2].trim() };
  }
  return { recipient: "all", content: trimmed };
}

export function registerCollabCommands(
  context: vscode.ExtensionContext,
  deps: CommandDeps
): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("collab.sendMessage", async () => {
      const cfg = deps.getConfig();
      if (!cfg.instance) {
        void vscode.window.showErrorMessage("Set collab.instance to send messages.");
        return;
      }
      const text = await vscode.window.showInputBox({
        prompt: "Message: @recipient text for DM, or plain text for broadcast",
        placeHolder: "@backend Deploy is ready",
      });
      if (text === undefined) {
        return;
      }
      const parsed = parseSendInput(text);
      if (!parsed || !parsed.content) {
        void vscode.window.showWarningMessage("Invalid message.");
        return;
      }
      try {
        const msg = await deps
          .getApi()
          .postMessage(cfg.instance, parsed.recipient, parsed.content, []);
        void vscode.window.showInformationMessage(`Sent (hash ${msg.hash.slice(0, 8)}…)`);
      } catch (e) {
        const err = e instanceof Error ? e.message : String(e);
        void vscode.window.showErrorMessage(`Send failed: ${err}`);
      }
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("collab.checkMessages", async () => {
      const cfg = deps.getConfig();
      if (!cfg.instance) {
        void vscode.window.showErrorMessage("Set collab.instance to check messages.");
        return;
      }
      deps.outputChannel.clear();
      deps.outputChannel.show(true);
      try {
        const list = await deps.getApi().getMessages(cfg.instance);
        deps.outputChannel.appendLine(`Messages for @${cfg.instance} (server filter: last 8h):`);
        deps.outputChannel.appendLine("");
        if (list.length === 0) {
          deps.outputChannel.appendLine("(no messages)");
          return;
        }
        for (const m of list) {
          deps.outputChannel.appendLine("─".repeat(40));
          deps.outputChannel.appendLine(`From: @${m.sender}  To: @${m.recipient}`);
          deps.outputChannel.appendLine(`Time: ${m.timestamp}`);
          deps.outputChannel.appendLine(`Hash: ${m.hash}`);
          deps.outputChannel.appendLine(m.content);
        }
      } catch (e) {
        const err = e instanceof Error ? e.message : String(e);
        deps.outputChannel.appendLine(`Error: ${err}`);
      }
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("collab.showRoster", async () => {
      await vscode.commands.executeCommand("workbench.view.extension.collab");
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("collab.openChat", (prefill?: string) => {
      deps.onOpenChat(typeof prefill === "string" ? prefill : undefined);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("collab.showUsage", () => {
      deps.outputChannel.clear();
      deps.outputChannel.show(true);
      const folders = vscode.workspace.workspaceFolders;
      if (!folders?.length) {
        deps.outputChannel.appendLine("No workspace folder — open a project to read .collab/usage.log.");
        return;
      }
      const logPath = path.join(folders[0].uri.fsPath, ".collab", "usage.log");
      try {
        if (!fs.existsSync(logPath)) {
          deps.outputChannel.appendLine(`File not found: ${logPath}`);
          return;
        }
        const raw = fs.readFileSync(logPath, "utf8");
        deps.outputChannel.appendLine(`— ${logPath} —`);
        deps.outputChannel.appendLine("");
        deps.outputChannel.appendLine(raw);
      } catch (e) {
        const err = e instanceof Error ? e.message : String(e);
        deps.outputChannel.appendLine(`Error reading usage log: ${err}`);
      }
    })
  );
}

export function openChatPanel(
  extensionUri: vscode.Uri,
  getApi: () => CollabApi,
  getInstance: () => string,
  prefill?: string
): void {
  CollabChatPanel.createOrShow(extensionUri, getApi, getInstance, prefill);
}
