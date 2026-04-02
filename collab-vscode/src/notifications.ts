import * as vscode from "vscode";
import type { Message } from "./api";

export function notifyIncomingMessage(
  msg: Message,
  myInstance: string,
  openChat: () => void
): void {
  if (msg.sender === myInstance) {
    return;
  }
  const toMe = msg.recipient === myInstance;
  const broadcast = msg.recipient === "all";
  if (!toMe && !broadcast) {
    return;
  }
  const preview = msg.content.length > 100 ? `${msg.content.slice(0, 100)}…` : msg.content;
  const title = broadcast ? `[broadcast] ${msg.sender}` : `${msg.sender} → @${msg.recipient}`;
  void vscode.window
    .showInformationMessage(`${title}: ${preview}`, "Open Chat")
    .then((choice) => {
      if (choice === "Open Chat") {
        openChat();
      }
    });
}
