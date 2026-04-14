import Foundation

struct Worker: Codable, Identifiable {
    let instanceId: String
    let role: String?
    let lastSeen: String?
    let messageCount: Int?

    var id: String { instanceId }

    enum CodingKeys: String, CodingKey {
        case instanceId = "instance_id"
        case role
        case lastSeen = "last_seen"
        case messageCount = "message_count"
    }

    var isOnline: Bool {
        guard let ls = lastSeen,
              let date = ISO8601DateFormatter().date(from: ls)
        else { return false }
        return Date().timeIntervalSince(date) < 90
    }
}
