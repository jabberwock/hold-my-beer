import * as vscode from "vscode";
import type { CollabApi } from "./api";
import type { WorkerInfo } from "./api";

const ONLINE_MS = 2 * 60 * 1000;

export class RosterItem extends vscode.TreeItem {
  constructor(
    public readonly worker: WorkerInfo,
    online: boolean
  ) {
    super(worker.instance_id, vscode.TreeItemCollapsibleState.None);
    this.description = worker.role || "—";
    this.tooltip = new vscode.MarkdownString(
      `**${worker.instance_id}**\n\n${worker.role || "(no role)"}\n\nLast seen: ${worker.last_seen}`
    );
    this.contextValue = online ? "collabOnline" : "collabOffline";
    this.iconPath = new vscode.ThemeIcon(
      "circle-filled",
      online
        ? new vscode.ThemeColor("charts.green")
        : new vscode.ThemeColor("charts.grey")
    );
    this.command = {
      command: "collab.openChat",
      title: "Open chat",
      arguments: [`@${worker.instance_id} `],
    };
  }
}

function isOnline(lastSeenIso: string): boolean {
  const t = Date.parse(lastSeenIso);
  if (Number.isNaN(t)) {
    return false;
  }
  return Date.now() - t < ONLINE_MS;
}

export class RosterProvider implements vscode.TreeDataProvider<RosterItem> {
  private _onDidChangeTreeData = new vscode.EventEmitter<RosterItem | undefined | null | void>();
  readonly onDidChangeTreeData = this._onDidChangeTreeData.event;

  private cache: WorkerInfo[] = [];

  constructor(private readonly getApi: () => CollabApi) {}

  refresh(): void {
    this._onDidChangeTreeData.fire();
  }

  async loadFromServer(): Promise<void> {
    const api = this.getApi();
    this.cache = await api.getRoster();
    this.refresh();
  }

  getTreeItem(element: RosterItem): vscode.TreeItem {
    return element;
  }

  async getChildren(): Promise<RosterItem[]> {
    if (this.cache.length === 0) {
      try {
        await this.loadFromServer();
      } catch {
        return [];
      }
    }
    return this.cache.map((w) => new RosterItem(w, isOnline(w.last_seen)));
  }

  get workers(): WorkerInfo[] {
    return this.cache;
  }

  countOnline(): number {
    return this.cache.filter((w) => isOnline(w.last_seen)).length;
  }
}
