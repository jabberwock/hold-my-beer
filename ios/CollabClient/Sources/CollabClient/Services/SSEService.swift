import Foundation

/// Streams Server-Sent Events from /events using URLSession.
/// Delivers parsed Message objects on the main actor.
@MainActor
final class SSEService: NSObject, ObservableObject {
    @Published var isConnected = false

    private var config: AppConfig
    private var session: URLSession?
    private var task: URLSessionDataTask?
    private var buffer = ""
    private var retryCount = 0
    private var retryTask: Task<Void, Never>?

    var onMessage: ((Message) -> Void)?

    init(config: AppConfig) {
        self.config = config
        super.init()
    }

    func update(config: AppConfig) {
        self.config = config
    }

    func connect() {
        disconnect()
        guard let base = config.baseURL,
              let url = URL(string: "/events", relativeTo: base)
        else { return }

        var req = URLRequest(url: url)
        req.setValue("Bearer \(config.token)", forHTTPHeaderField: "Authorization")
        req.setValue("text/event-stream", forHTTPHeaderField: "Accept")
        req.timeoutInterval = 86400 // 24h — stream stays open

        let sessionConfig = URLSessionConfiguration.default
        sessionConfig.timeoutIntervalForResource = 86400
        session = URLSession(configuration: sessionConfig, delegate: self, delegateQueue: nil)
        task = session?.dataTask(with: req)
        task?.resume()
    }

    func disconnect() {
        retryTask?.cancel()
        retryTask = nil
        task?.cancel()
        task = nil
        session?.invalidateAndCancel()
        session = nil
        buffer = ""
        isConnected = false
    }

    private func scheduleReconnect() {
        let delay = min(1.0 * pow(1.8, Double(retryCount)) + Double.random(in: 0...0.5), 30.0)
        retryCount += 1
        retryTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard !Task.isCancelled else { return }
            await self?.connect()
        }
    }

    private func processBuffer() {
        // SSE blocks separated by \n\n
        let parts = buffer.components(separatedBy: "\n\n")
        buffer = parts.last ?? ""
        for block in parts.dropLast() {
            for line in block.components(separatedBy: "\n") {
                if line.hasPrefix("data: ") {
                    let json = String(line.dropFirst(6))
                    if let data = json.data(using: .utf8),
                       let msg = try? JSONDecoder().decode(Message.self, from: data) {
                        onMessage?(msg)
                    }
                }
            }
        }
    }
}

extension SSEService: URLSessionDataDelegate {
    nonisolated func urlSession(_ session: URLSession, dataTask: URLSessionDataTask,
                    didReceive response: URLResponse,
                    completionHandler: @escaping (URLSession.ResponseDisposition) -> Void) {
        Task { @MainActor [weak self] in
            if let http = response as? HTTPURLResponse, (200..<300).contains(http.statusCode) {
                self?.isConnected = true
                self?.retryCount = 0
            }
        }
        completionHandler(.allow)
    }

    nonisolated func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive data: Data) {
        guard let text = String(data: data, encoding: .utf8) else { return }
        Task { @MainActor [weak self] in
            self?.buffer += text
            self?.processBuffer()
        }
    }

    nonisolated func urlSession(_ session: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        Task { @MainActor [weak self] in
            self?.isConnected = false
            // Don't reconnect if we intentionally cancelled
            if let err = error as? URLError, err.code == .cancelled { return }
            self?.scheduleReconnect()
        }
    }
}
