import * as crypto from "crypto";
import * as vscode from "vscode";
import type { CollabApi } from "./api";
import type { Message } from "./api";

export class CollabChatPanel {
  public static current: CollabChatPanel | undefined;

  private readonly panel: vscode.WebviewPanel;
  private readonly disposables: vscode.Disposable[] = [];
  private hashes = new Set<string>();

  private constructor(
    panel: vscode.WebviewPanel,
    private readonly getApi: () => CollabApi,
    private readonly getInstance: () => string
  ) {
    this.panel = panel;
    CollabChatPanel.current = this;
    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
    this.panel.webview.onDidReceiveMessage(
      (m: { type?: string; text?: string }) => {
        if (m.type === "send" && typeof m.text === "string") {
          void this.handleSend(m.text);
        }
      },
      null,
      this.disposables
    );
  }

  static createOrShow(
    _extensionUri: vscode.Uri,
    getApi: () => CollabApi,
    getInstance: () => string,
    prefill?: string
  ): CollabChatPanel {
    const column = vscode.window.activeTextEditor?.viewColumn ?? vscode.ViewColumn.One;
    if (CollabChatPanel.current) {
      CollabChatPanel.current.panel.reveal(column);
      if (prefill) {
        CollabChatPanel.current.postPrefill(prefill);
      }
      return CollabChatPanel.current;
    }
    const panel = vscode.window.createWebviewPanel(
      "collabChat",
      "Collab Chat",
      column,
      {
        enableScripts: true,
        retainContextWhenHidden: true,
      }
    );
    const chat = new CollabChatPanel(panel, getApi, getInstance);
    chat.panel.webview.html = chat.getHtml(panel.webview);
    void chat.bootstrap();
    if (prefill) {
      chat.postPrefill(prefill);
    }
    return chat;
  }

  private postPrefill(text: string): void {
    void this.panel.webview.postMessage({ type: "prefill", text });
  }

  private async bootstrap(): Promise<void> {
    const api = this.getApi();
    const me = this.getInstance();
    if (!me) {
      void this.panel.webview.postMessage({
        type: "init",
        messages: [],
        error: "Set collab.instance (or COLLAB_INSTANCE) to load messages.",
      });
      return;
    }
    try {
      const roster = await api.getRoster();
      const history = await api.getHistory(me);
      const seen = new Set<string>();
      const merged: Message[] = [];
      for (const m of history) {
        if (!seen.has(m.hash)) {
          seen.add(m.hash);
          merged.push(m);
        }
      }
      merged.sort((a, b) => {
        const ta = Date.parse(String(a.timestamp));
        const tb = Date.parse(String(b.timestamp));
        return ta - tb;
      });
      this.hashes = new Set(merged.map((m) => m.hash));
      void this.panel.webview.postMessage({
        type: "init",
        messages: merged,
        roster: roster.map((r) => r.instance_id),
      });
    } catch (e) {
      const err = e instanceof Error ? e.message : String(e);
      void this.panel.webview.postMessage({
        type: "init",
        messages: [],
        error: err,
      });
    }
  }

  private async handleSend(text: string): Promise<void> {
    const trimmed = text.trim();
    if (!trimmed) {
      return;
    }
    const me = this.getInstance();
    if (!me) {
      void vscode.window.showErrorMessage("collab.instance is not set.");
      return;
    }
    let recipient: string;
    let content: string;
    const dm = /^@([\w-]+)\s*(.*)$/s.exec(trimmed);
    if (dm) {
      recipient = dm[1];
      content = dm[2].trim();
      if (!content) {
        void vscode.window.showWarningMessage("Add a message after @recipient");
        return;
      }
    } else {
      recipient = "all";
      content = trimmed;
    }
    try {
      const api = this.getApi();
      const sent = await api.postMessage(me, recipient, content, []);
      if (!this.hashes.has(sent.hash)) {
        this.hashes.add(sent.hash);
        void this.panel.webview.postMessage({ type: "append", message: sent });
      }
    } catch (e) {
      const err = e instanceof Error ? e.message : String(e);
      void vscode.window.showErrorMessage(`Send failed: ${err}`);
    }
  }

  appendMessage(msg: Message): void {
    if (this.hashes.has(msg.hash)) {
      return;
    }
    this.hashes.add(msg.hash);
    void this.panel.webview.postMessage({ type: "append", message: msg });
  }

  reveal(): void {
    const column = vscode.window.activeTextEditor?.viewColumn ?? vscode.ViewColumn.One;
    this.panel.reveal(column);
  }

  private getHtml(webview: vscode.Webview): string {
    const nonce = crypto.randomBytes(16).toString("base64");
    const csp = [
      `default-src 'none'`,
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `script-src 'nonce-${nonce}'`,
    ].join("; ");
    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="${csp}" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <style>
    :root {
      --bg: #1e1e1e;
      --fg: #d4d4d4;
      --muted: #858585;
      --border: #3c3c3c;
      --accent: #3794ff;
    }
    body {
      margin: 0;
      padding: 0;
      font-family: var(--vscode-font-family), system-ui, sans-serif;
      font-size: 13px;
      background: var(--bg);
      color: var(--fg);
      display: flex;
      flex-direction: column;
      height: 100vh;
      box-sizing: border-box;
    }
    #meta {
      padding: 8px 12px;
      border-bottom: 1px solid var(--border);
      color: var(--muted);
      font-size: 11px;
    }
    #feed {
      flex: 1;
      overflow-y: auto;
      padding: 12px;
      gap: 10px;
      display: flex;
      flex-direction: column;
    }
    .msg {
      border-left: 2px solid var(--border);
      padding: 6px 10px;
      background: rgba(255,255,255,0.03);
    }
    .msg-head {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      align-items: baseline;
      margin-bottom: 4px;
      font-size: 11px;
      color: var(--muted);
    }
    .tag {
      display: inline-block;
      padding: 1px 6px;
      border-radius: 4px;
      background: rgba(55, 148, 255, 0.2);
      color: var(--accent);
      font-weight: 600;
      font-size: 10px;
    }
    .body {
      white-space: pre-wrap;
      word-break: break-word;
      line-height: 1.45;
    }
    #composer {
      border-top: 1px solid var(--border);
      padding: 10px 12px;
      display: flex;
      gap: 8px;
      align-items: flex-end;
    }
    #input {
      flex: 1;
      min-height: 36px;
      max-height: 120px;
      resize: vertical;
      background: var(--bg);
      color: var(--fg);
      border: 1px solid var(--border);
      border-radius: 4px;
      padding: 8px;
      font-family: inherit;
      font-size: 13px;
    }
    #input:focus {
      outline: 1px solid var(--accent);
    }
    .hint {
      font-size: 11px;
      color: var(--muted);
      padding: 0 12px 8px;
    }
  </style>
</head>
<body>
  <div id="meta">Loading…</div>
  <div id="feed"></div>
  <div class="hint">@name message — direct message · plain text — broadcast to team</div>
  <div id="composer">
    <textarea id="input" rows="2" placeholder="Message…"></textarea>
  </div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const feed = document.getElementById('feed');
    const meta = document.getElementById('meta');
    const input = document.getElementById('input');

    function esc(s) {
      return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;');
    }

    function fmtTime(iso) {
      try {
        const d = new Date(iso);
        return d.toLocaleString(undefined, { month:'short', day:'numeric', hour:'2-digit', minute:'2-digit', second:'2-digit' });
      } catch { return iso; }
    }

    function renderMsg(m) {
      const div = document.createElement('div');
      div.className = 'msg';
      const broadcast = m.recipient === 'all';
      const head = document.createElement('div');
      head.className = 'msg-head';
      head.innerHTML =
        '<span><strong>' + esc(m.sender) + '</strong> → ' + esc(m.recipient) + '</span>' +
        (broadcast ? '<span class="tag">broadcast</span>' : '') +
        '<span>' + esc(fmtTime(m.timestamp)) + '</span>';
      const body = document.createElement('div');
      body.className = 'body';
      body.textContent = m.content;
      div.appendChild(head);
      div.appendChild(body);
      return div;
    }

    window.addEventListener('message', (event) => {
      const msg = event.data;
      if (msg.type === 'init') {
        feed.innerHTML = '';
        if (msg.error) {
          meta.textContent = 'Error: ' + msg.error;
        } else {
          const r = (msg.roster && msg.roster.length) ? ('Roster: ' + msg.roster.join(', ')) : 'Collab';
          meta.textContent = r;
        }
        (msg.messages || []).forEach((m) => feed.appendChild(renderMsg(m)));
        feed.scrollTop = feed.scrollHeight;
      }
      if (msg.type === 'append') {
        feed.appendChild(renderMsg(msg.message));
        feed.scrollTop = feed.scrollHeight;
      }
      if (msg.type === 'prefill' && typeof msg.text === 'string') {
        input.value = msg.text;
        input.focus();
      }
    });

    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        const t = input.value;
        input.value = '';
        vscode.postMessage({ type: 'send', text: t });
      }
    });
  </script>
</body>
</html>`;
  }

  dispose(): void {
    CollabChatPanel.current = undefined;
    this.panel.dispose();
    while (this.disposables.length) {
      const d = this.disposables.pop();
      d?.dispose();
    }
  }
}
