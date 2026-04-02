import type { CollabResolvedConfig } from "./config";

export interface Message {
  id: string;
  hash: string;
  sender: string;
  recipient: string;
  content: string;
  refs: string[];
  timestamp: string;
}

export interface WorkerInfo {
  instance_id: string;
  role: string;
  last_seen: string;
  message_count?: number;
}

export interface Todo {
  id: string;
  hash: string;
  instance: string;
  assigned_by: string;
  description: string;
  created_at: string;
  completed_at?: string | null;
}

function authHeaders(token: string): Record<string, string> {
  return {
    Authorization: `Bearer ${token}`,
    "Content-Type": "application/json",
  };
}

async function handleJson<T>(res: Response): Promise<T> {
  const text = await res.text();
  if (!res.ok) {
    throw new Error(`HTTP ${res.status}: ${text.slice(0, 200)}`);
  }
  if (!text) {
    return {} as T;
  }
  return JSON.parse(text) as T;
}

export class CollabApi {
  constructor(private cfg: CollabResolvedConfig) {}

  private base(path: string): string {
    return `${this.cfg.server}${path}`;
  }

  async getRoster(): Promise<WorkerInfo[]> {
    const res = await fetch(this.base("/roster"), {
      headers: authHeaders(this.cfg.token),
    });
    return handleJson<WorkerInfo[]>(res);
  }

  async getMessages(instanceId: string): Promise<Message[]> {
    const res = await fetch(this.base(`/messages/${encodeURIComponent(instanceId)}`), {
      headers: authHeaders(this.cfg.token),
    });
    return handleJson<Message[]>(res);
  }

  /** Full conversation (sent + received) — used for chat feed ordering. */
  async getHistory(instanceId: string): Promise<Message[]> {
    const res = await fetch(this.base(`/history/${encodeURIComponent(instanceId)}`), {
      headers: authHeaders(this.cfg.token),
    });
    return handleJson<Message[]>(res);
  }

  async postMessage(
    sender: string,
    recipient: string,
    content: string,
    refs: string[] = []
  ): Promise<Message> {
    const res = await fetch(this.base("/messages"), {
      method: "POST",
      headers: authHeaders(this.cfg.token),
      body: JSON.stringify({ sender, recipient, content, refs }),
    });
    return handleJson<Message>(res);
  }

  async putPresence(instanceId: string, role: string): Promise<void> {
    const res = await fetch(this.base(`/presence/${encodeURIComponent(instanceId)}`), {
      method: "PUT",
      headers: authHeaders(this.cfg.token),
      body: JSON.stringify({ role }),
    });
    if (!res.ok) {
      const text = await res.text();
      throw new Error(`HTTP ${res.status}: ${text.slice(0, 200)}`);
    }
  }

  async deletePresence(instanceId: string): Promise<void> {
    const res = await fetch(this.base(`/presence/${encodeURIComponent(instanceId)}`), {
      method: "DELETE",
      headers: { Authorization: `Bearer ${this.cfg.token}` },
    });
    if (!res.ok && res.status !== 404) {
      const text = await res.text();
      throw new Error(`HTTP ${res.status}: ${text.slice(0, 200)}`);
    }
  }

  async listTodos(instanceId: string): Promise<Todo[]> {
    const res = await fetch(this.base(`/todos/${encodeURIComponent(instanceId)}`), {
      headers: authHeaders(this.cfg.token),
    });
    return handleJson<Todo[]>(res);
  }

  async createTodo(
    assignedBy: string,
    instance: string,
    description: string
  ): Promise<Todo> {
    const res = await fetch(this.base("/todos"), {
      method: "POST",
      headers: authHeaders(this.cfg.token),
      body: JSON.stringify({
        assigned_by: assignedBy,
        instance,
        description,
      }),
    });
    return handleJson<Todo>(res);
  }

  async completeTodo(hashPrefix: string): Promise<void> {
    const res = await fetch(
      this.base(`/todos/${encodeURIComponent(hashPrefix)}/done`),
      {
        method: "PATCH",
        headers: { Authorization: `Bearer ${this.cfg.token}` },
      }
    );
    if (!res.ok && res.status !== 204) {
      const text = await res.text();
      throw new Error(`HTTP ${res.status}: ${text.slice(0, 200)}`);
    }
  }
}
