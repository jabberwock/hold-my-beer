import Foundation

struct Metrics: Codable {
    let totalMessages: Int?
    let activeWorkers: Int?
    let uptimeSeconds: Double?
    let messagesPerHour: Double?

    enum CodingKeys: String, CodingKey {
        case totalMessages = "total_messages"
        case activeWorkers = "active_workers"
        case uptimeSeconds = "uptime_seconds"
        case messagesPerHour = "messages_per_hour"
    }

    var uptimeFormatted: String {
        guard let s = uptimeSeconds else { return "—" }
        let h = Int(s) / 3600
        let m = (Int(s) % 3600) / 60
        if h > 0 { return "\(h)h \(m)m" }
        return "\(m)m"
    }
}
