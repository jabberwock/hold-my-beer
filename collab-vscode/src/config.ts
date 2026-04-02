import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

export interface CollabResolvedConfig {
  server: string;
  token: string;
  instance: string;
}

function parseEnvFile(contents: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of contents.split("\n")) {
    const t = line.trim();
    if (!t || t.startsWith("#")) {
      continue;
    }
    const eq = t.indexOf("=");
    if (eq <= 0) {
      continue;
    }
    const key = t.slice(0, eq).trim();
    let val = t.slice(eq + 1).trim();
    if (
      (val.startsWith('"') && val.endsWith('"')) ||
      (val.startsWith("'") && val.endsWith("'"))
    ) {
      val = val.slice(1, -1);
    }
    out[key] = val;
  }
  return out;
}

function loadWorkspaceDotEnv(): Record<string, string> {
  const folders = vscode.workspace.workspaceFolders;
  if (!folders?.length) {
    return {};
  }
  const root = folders[0].uri.fsPath;
  const candidate = path.join(root, ".env");
  try {
    if (fs.existsSync(candidate)) {
      return parseEnvFile(fs.readFileSync(candidate, "utf8"));
    }
  } catch {
    // ignore
  }
  return {};
}

function envOr(
  key: string,
  dot: Record<string, string>,
  fallback: string
): string {
  const fromProcess = process.env[key];
  if (fromProcess !== undefined && fromProcess !== "") {
    return fromProcess;
  }
  const fromDot = dot[key];
  if (fromDot !== undefined && fromDot !== "") {
    return fromDot;
  }
  return fallback;
}

export function getCollabConfig(): CollabResolvedConfig {
  const ws = vscode.workspace.getConfiguration("collab");
  const dot = loadWorkspaceDotEnv();

  const server = (ws.get<string>("server")?.trim() ||
    envOr("COLLAB_SERVER", dot, "http://localhost:8000")) as string;

  const token = (ws.get<string>("token")?.trim() ||
    envOr("COLLAB_TOKEN", dot, "")) as string;

  const instance = (ws.get<string>("instance")?.trim() ||
    envOr("COLLAB_INSTANCE", dot, "")) as string;

  return { server: server.replace(/\/$/, ""), token, instance };
}
