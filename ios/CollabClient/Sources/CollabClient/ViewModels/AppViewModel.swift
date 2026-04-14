import Foundation
import SwiftUI

@MainActor
final class AppViewModel: ObservableObject {
    @Published var config: AppConfig = AppConfig.load()
    @Published var messages: [Message] = []
    @Published var roster: [Worker] = []
    @Published var todos: [TodoItem] = []
    @Published var metrics: Metrics?
    @Published var isConnected = false
    @Published var activeTab: FeedTab = .all
    @Published var unreadMentions = 0
    @Published var errorMessage: String?
    @Published var showingError = false

    enum FeedTab { case all, mentions }

    var filteredMessages: [Message] {
        switch activeTab {
        case .all: return messages
        case .mentions: return messages.filter { $0.recipient == config.identity }
        }
    }

    var workerNames: [String] {
        var names: [String] = ["all"]
        names += roster.map(\.instanceId)
        return names
    }

    var api: CollabAPI
    private var sseService: SSEService
    private var rosterTimer: Task<Void, Never>?
    private var todosTimer: Task<Void, Never>?
    private var presenceTimer: Task<Void, Never>?

    init() {
        let cfg = AppConfig.load()
        self.config = cfg
        self.api = CollabAPI(config: cfg)
        self.sseService = SSEService(config: cfg)
        self.sseService.onMessage = { [weak self] msg in
            Task { @MainActor [weak self] in
                self?.handleIncomingMessage(msg)
            }
        }
        // Observe SSE connection state
        Task { @MainActor in
            for await connected in sseService.$isConnected.values {
                self.isConnected = connected
            }
        }
    }

    func applyConfig(_ cfg: AppConfig) {
        config = cfg
        cfg.save()
        api.config = cfg
        sseService.update(config: cfg)
    }

    // MARK: - Dashboard lifecycle

    func startDashboard() {
        loadInitialMessages()
        sseService.connect()
        startPolling()
        Task { try? await api.updatePresence(instance: config.identity, role: "GUI observer") }
    }

    func stopDashboard() {
        sseService.disconnect()
        rosterTimer?.cancel()
        todosTimer?.cancel()
        presenceTimer?.cancel()
        rosterTimer = nil
        todosTimer = nil
        presenceTimer = nil
    }

    private func startPolling() {
        rosterTimer = Task { [weak self] in
            while !Task.isCancelled {
                await self?.fetchRoster()
                try? await Task.sleep(nanoseconds: 30_000_000_000)
            }
        }
        todosTimer = Task { [weak self] in
            while !Task.isCancelled {
                await self?.fetchTodos()
                try? await Task.sleep(nanoseconds: 15_000_000_000)
            }
        }
        presenceTimer = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 60_000_000_000)
                guard let self else { return }
                try? await api.updatePresence(instance: config.identity, role: "GUI observer")
            }
        }
    }

    // MARK: - Data loading

    private func loadInitialMessages() {
        Task {
            do {
                let history = try await api.fetchHistory(for: config.identity, limit: 200)
                // Also fetch all broadcast messages
                let allHistory = try await api.fetchHistory(for: "all", limit: 200)
                let combined = (history + allHistory)
                    .sorted { $0.date < $1.date }
                var seen = Set<Int>()
                messages = combined.filter { seen.insert($0.id).inserted }
            } catch {
                showError(error.localizedDescription)
            }
        }
    }

    func fetchRoster() async {
        do {
            roster = try await api.fetchRoster()
        } catch {}
    }

    func fetchTodos() async {
        let workerIds = Set(roster.map(\.instanceId) + [config.identity])
        var allTodos: [TodoItem] = []
        await withTaskGroup(of: [TodoItem].self) { group in
            for id in workerIds {
                group.addTask {
                    (try? await self.api.fetchTodos(for: id)) ?? []
                }
            }
            for await items in group { allTodos += items }
        }
        todos = allTodos.sorted { $0.date < $1.date }
    }

    func fetchMetrics() async {
        metrics = try? await api.fetchMetrics()
    }

    // MARK: - Messages

    private func handleIncomingMessage(_ msg: Message) {
        guard !messages.contains(where: { $0.id == msg.id }) else { return }
        messages.append(msg)
        if msg.recipient == config.identity && activeTab != .mentions {
            unreadMentions += 1
        }
    }

    func sendMessage(to recipient: String, content: String) async throws {
        let req = CreateMessageRequest(sender: config.identity, recipient: recipient, content: content)
        try await api.sendMessage(req)
    }

    func setTab(_ tab: FeedTab) {
        activeTab = tab
        if tab == .mentions { unreadMentions = 0 }
    }

    // MARK: - Todos

    func addTodo(instance: String, description: String) async throws {
        let req = CreateTodoRequest(assignedBy: config.identity, instance: instance, description: description)
        try await api.createTodo(req)
        await fetchTodos()
    }

    func completeTodo(hash: String) async throws {
        try await api.completeTodo(hash: hash)
        await fetchTodos()
    }

    // MARK: - Helpers

    func showError(_ msg: String) {
        errorMessage = msg
        showingError = true
    }

    func lastSenderForMe() -> String? {
        let me = config.identity
        for msg in messages.reversed() {
            if msg.sender != me && (msg.recipient == me || msg.isToAll) { return msg.sender }
        }
        return messages.reversed().first(where: { $0.sender != me })?.sender
    }
}

// MARK: - Color palette (matches web GUI)
let workerColors: [Color] = [
    Color(red: 0.29, green: 0.56, blue: 1.0),   // blue
    Color(red: 0.4,  green: 0.8,  blue: 0.4),   // green
    Color(red: 0.9,  green: 0.5,  blue: 0.2),   // orange
    Color(red: 0.8,  green: 0.3,  blue: 0.8),   // purple
    Color(red: 0.9,  green: 0.3,  blue: 0.3),   // red
    Color(red: 0.3,  green: 0.8,  blue: 0.8),   // cyan
]

private var colorAssignments: [String: Int] = [:]
private var colorCounter = 0

func colorForSender(_ sender: String) -> Color {
    if let idx = colorAssignments[sender] { return workerColors[idx] }
    let idx = colorCounter % workerColors.count
    colorAssignments[sender] = idx
    colorCounter += 1
    return workerColors[idx]
}
