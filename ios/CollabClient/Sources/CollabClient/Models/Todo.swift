import Foundation

struct TodoItem: Codable, Identifiable {
    let id: Int
    let hash: String
    let instance: String
    let assignedBy: String
    let description: String
    let createdAt: String

    enum CodingKeys: String, CodingKey {
        case id, hash, instance, description
        case assignedBy = "assigned_by"
        case createdAt = "created_at"
    }

    var date: Date {
        ISO8601DateFormatter().date(from: createdAt) ?? Date()
    }

    var timeAgo: String {
        let elapsed = -date.timeIntervalSinceNow
        if elapsed < 60 { return "just now" }
        if elapsed < 3600 { return "\(Int(elapsed / 60))m ago" }
        if elapsed < 86400 { return "\(Int(elapsed / 3600))h ago" }
        return "\(Int(elapsed / 86400))d ago"
    }
}

struct CreateTodoRequest: Codable {
    let assignedBy: String
    let instance: String
    let description: String

    enum CodingKeys: String, CodingKey {
        case instance, description
        case assignedBy = "assigned_by"
    }
}
