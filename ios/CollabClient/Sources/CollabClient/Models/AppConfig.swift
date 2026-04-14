import Foundation

struct AppConfig: Codable {
    var token: String = ""
    var serverURL: String = "http://localhost:8000"
    var identity: String = "human"
    var setupComplete: Bool = false

    static let key = "collab_config"

    static func load() -> AppConfig {
        guard let data = UserDefaults.standard.data(forKey: key),
              let cfg = try? JSONDecoder().decode(AppConfig.self, from: data)
        else { return AppConfig() }
        return cfg
    }

    func save() {
        if let data = try? JSONEncoder().encode(self) {
            UserDefaults.standard.set(data, forKey: AppConfig.key)
        }
    }

    var baseURL: URL? { URL(string: serverURL) }

    var authHeader: [String: String] {
        ["Authorization": "Bearer \(token)"]
    }
}
