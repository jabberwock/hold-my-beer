import Foundation

enum APIError: LocalizedError {
    case badURL
    case httpError(Int)
    case decodingError(Error)
    case networkError(Error)
    case unauthorized

    var errorDescription: String? {
        switch self {
        case .badURL: return "Invalid server URL"
        case .httpError(let code): return "Server error: HTTP \(code)"
        case .decodingError(let e): return "Parse error: \(e.localizedDescription)"
        case .networkError(let e): return "Network error: \(e.localizedDescription)"
        case .unauthorized: return "Invalid token — check your credentials"
        }
    }
}

@MainActor
final class CollabAPI: ObservableObject {
    var config: AppConfig

    init(config: AppConfig) {
        self.config = config
    }

    // MARK: - Generic request helpers

    private func url(_ path: String) throws -> URL {
        guard let base = config.baseURL,
              let url = URL(string: path, relativeTo: base)
        else { throw APIError.badURL }
        return url
    }

    private func get<T: Decodable>(_ path: String) async throws -> T {
        let req = try makeRequest(path: path, method: "GET")
        return try await perform(req)
    }

    private func post<B: Encodable, T: Decodable>(_ path: String, body: B) async throws -> T {
        var req = try makeRequest(path: path, method: "POST")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONEncoder().encode(body)
        return try await perform(req)
    }

    private func postNoBody(_ path: String) async throws {
        var req = try makeRequest(path: path, method: "POST")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let (_, resp) = try await URLSession.shared.data(for: req)
        if let http = resp as? HTTPURLResponse, http.statusCode == 401 { throw APIError.unauthorized }
        if let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw APIError.httpError(http.statusCode)
        }
    }

    private func patch(_ path: String) async throws {
        var req = try makeRequest(path: path, method: "PATCH")
        let (_, resp) = try await URLSession.shared.data(for: req)
        if let http = resp as? HTTPURLResponse, http.statusCode == 401 { throw APIError.unauthorized }
        if let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw APIError.httpError(http.statusCode)
        }
    }

    private func put<B: Encodable>(_ path: String, body: B) async throws {
        var req = try makeRequest(path: path, method: "PUT")
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONEncoder().encode(body)
        let (_, resp) = try await URLSession.shared.data(for: req)
        if let http = resp as? HTTPURLResponse, http.statusCode == 401 { throw APIError.unauthorized }
        if let http = resp as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
            throw APIError.httpError(http.statusCode)
        }
    }

    private func makeRequest(path: String, method: String) throws -> URLRequest {
        let u = try url(path)
        var req = URLRequest(url: u, timeoutInterval: 15)
        req.httpMethod = method
        for (k, v) in config.authHeader { req.setValue(v, forHTTPHeaderField: k) }
        return req
    }

    private func perform<T: Decodable>(_ req: URLRequest) async throws -> T {
        do {
            let (data, resp) = try await URLSession.shared.data(for: req)
            if let http = resp as? HTTPURLResponse {
                if http.statusCode == 401 { throw APIError.unauthorized }
                if !(200..<300).contains(http.statusCode) { throw APIError.httpError(http.statusCode) }
            }
            do {
                return try JSONDecoder().decode(T.self, from: data)
            } catch {
                throw APIError.decodingError(error)
            }
        } catch let e as APIError {
            throw e
        } catch {
            throw APIError.networkError(error)
        }
    }

    // MARK: - Public API

    func fetchRoster() async throws -> [Worker] {
        try await get("/roster")
    }

    func fetchMessages(for instance: String) async throws -> [Message] {
        try await get("/messages/\(instance)")
    }

    func fetchHistory(for instance: String, limit: Int = 200) async throws -> [Message] {
        try await get("/history/\(instance)?limit=\(limit)")
    }

    func sendMessage(_ req: CreateMessageRequest) async throws {
        struct Empty: Decodable {}
        let _: Empty = try await post("/messages", body: req)
    }

    func fetchTodos(for instance: String) async throws -> [TodoItem] {
        try await get("/todos/\(instance)")
    }

    func createTodo(_ req: CreateTodoRequest) async throws {
        struct Empty: Decodable {}
        let _: Empty = try await post("/todos", body: req)
    }

    func completeTodo(hash: String) async throws {
        try await patch("/todos/\(hash)/done")
    }

    func fetchMetrics() async throws -> Metrics {
        try await get("/metrics")
    }

    func updatePresence(instance: String, role: String) async throws {
        struct PresenceBody: Encodable { let role: String; let status: String }
        try await put("/presence/\(instance)", body: PresenceBody(role: role, status: "active"))
    }

    func checkHealth() async -> Bool {
        guard let req = try? makeRequest(path: "/", method: "GET") else { return false }
        guard let (_, resp) = try? await URLSession.shared.data(for: req) else { return false }
        return (resp as? HTTPURLResponse)?.statusCode == 200
    }
}
