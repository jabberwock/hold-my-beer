import Foundation

struct Message: Codable, Identifiable, Equatable {
    let id: Int
    let hash: String
    let sender: String
    let recipient: String
    let content: String
    let timestamp: String

    var date: Date {
        ISO8601DateFormatter().date(from: timestamp) ?? Date()
    }

    var formattedTime: String {
        let d = date
        let formatter = DateFormatter()
        formatter.timeStyle = .short
        return formatter.string(from: d)
    }

    var isToAll: Bool { recipient == "all" }
}

struct CreateMessageRequest: Codable {
    let sender: String
    let recipient: String
    let content: String
}
