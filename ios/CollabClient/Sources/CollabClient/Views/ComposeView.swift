import SwiftUI

struct ComposeView: View {
    @ObservedObject var vm: AppViewModel
    @FocusState private var focused: Bool
    @State private var text = ""
    @State private var isSending = false
    @State private var errorMsg: String?
    @State private var mentionQuery = ""
    @State private var showMentions = false
    @State private var mentionMatches: [String] = []
    @State private var mentionIdx = 0
    @State private var mentionStart: String.Index?

    private let slashCommands: [(cmd: String, desc: String)] = [
        ("r",   "Reply to last sender"),
        ("w",   "DM a worker"),
        ("all", "Message everyone"),
    ]
    @State private var slashMatches: [(cmd: String, desc: String)] = []
    @State private var showSlash = false

    var body: some View {
        VStack(spacing: 0) {
            // Mention autocomplete list
            if showMentions && !mentionMatches.isEmpty {
                autocompleteList
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }

            // Slash command list
            if showSlash && !slashMatches.isEmpty {
                slashList
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }

            Divider()

            HStack(alignment: .bottom, spacing: 10) {
                TextField("Message… (@all, /r, /w)", text: $text, axis: .vertical)
                    .focused($focused)
                    .lineLimit(1...6)
                    .padding(.vertical, 8)
                    .onChange(of: text) { _, newVal in onTextChange(newVal) }

                Button(action: send) {
                    Image(systemName: "paperplane.fill")
                        .font(.system(size: 18))
                        .foregroundStyle(canSend ? .blue : .secondary)
                }
                .disabled(!canSend)
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 4)

            if let err = errorMsg {
                Text(err)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .padding(.horizontal, 14)
                    .padding(.bottom, 6)
            }
        }
        .background(Color(.systemBackground))
        .animation(.easeInOut(duration: 0.15), value: showMentions)
        .animation(.easeInOut(duration: 0.15), value: showSlash)
    }

    private var canSend: Bool { !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !isSending }

    // MARK: - Autocomplete UI

    private var autocompleteList: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                ForEach(Array(mentionMatches.enumerated()), id: \.offset) { idx, name in
                    Button("@\(name)") { applyMention(name) }
                        .font(.caption.bold())
                        .padding(.horizontal, 10)
                        .padding(.vertical, 5)
                        .background(idx == mentionIdx
                            ? Color.blue.opacity(0.15)
                            : Color(.secondarySystemBackground))
                        .foregroundStyle(idx == mentionIdx ? .blue : .primary)
                        .clipShape(Capsule())
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
        }
    }

    private var slashList: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(slashMatches, id: \.cmd) { item in
                Button(action: { applySlash(item.cmd) }) {
                    HStack {
                        Text("/\(item.cmd)")
                            .font(.system(.callout, design: .monospaced).bold())
                            .foregroundStyle(.blue)
                        Text(item.desc)
                            .font(.callout)
                            .foregroundStyle(.secondary)
                        Spacer()
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 8)
                }
            }
        }
        .background(Color(.secondarySystemBackground))
    }

    // MARK: - Input handling

    private func onTextChange(_ val: String) {
        errorMsg = nil

        // Slash commands (text starts with /)
        if val.hasPrefix("/"), !val.contains(" ") {
            let q = String(val.dropFirst()).lowercased()
            slashMatches = slashCommands.filter { $0.cmd.hasPrefix(q) }
            showSlash = !slashMatches.isEmpty
            showMentions = false
            return
        }

        // Auto-expand shortcuts on space
        if val == "/r " { applySlash("r"); return }
        if val == "/all " { applySlash("all"); return }
        if val == "/w " { applySlash("w"); return }

        showSlash = false

        // @ mention autocomplete
        // Find last @ before cursor
        if let atRange = findAtRange(in: val) {
            let query = String(val[atRange.lower...]).lowercased()
            let names = vm.workerNames
            let matches = names.filter { $0.lowercased().hasPrefix(query) }
            if !matches.isEmpty {
                mentionMatches = matches
                mentionStart = atRange.lower
                mentionIdx = 0
                showMentions = true
                return
            }
        }
        showMentions = false
    }

    private func findAtRange(in text: String) -> (lower: String.Index, upper: String.Index)? {
        // Walk backwards from end to find @word
        var idx = text.endIndex
        while idx > text.startIndex {
            let prev = text.index(before: idx)
            if text[prev] == "@" {
                // found @, return range from after @ to current end
                let wordStart = idx
                return (lower: wordStart, upper: text.endIndex)
            }
            if text[prev].isWhitespace { return nil }
            idx = prev
        }
        return nil
    }

    private func applyMention(_ name: String) {
        guard let start = mentionStart else { return }
        // Replace from @ onward with @name + space
        let atIdx = text.index(before: start)
        let before = String(text[..<atIdx])
        text = before + "@\(name) "
        showMentions = false
        mentionMatches = []
        mentionStart = nil
    }

    private func applySlash(_ cmd: String) {
        showSlash = false
        slashMatches = []
        switch cmd {
        case "r":
            if let sender = vm.lastSenderForMe() {
                text = "@\(sender) "
            } else {
                errorMsg = "No recent messages to reply to"
                text = ""
            }
        case "all":
            text = "@all "
        case "w":
            text = "@"
        default:
            break
        }
        focused = true
    }

    // MARK: - Send

    private func send() {
        var body = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty else { return }

        // Expand slash commands at send time
        if body.hasPrefix("/") {
            guard let expanded = expandSlash(body) else { return }
            body = expanded
        }

        // Parse recipient from leading @mention
        let (recipient, content) = parseMessage(body)

        isSending = true
        errorMsg = nil
        text = ""
        showMentions = false
        showSlash = false

        Task {
            do {
                try await vm.sendMessage(to: recipient, content: content)
            } catch {
                errorMsg = error.localizedDescription
            }
            isSending = false
        }
    }

    private func expandSlash(_ text: String) -> String? {
        let parts = text.dropFirst().components(separatedBy: " ")
        let cmd = parts.first ?? ""
        let rest = parts.dropFirst().joined(separator: " ")
        switch cmd {
        case "r":
            guard let sender = vm.lastSenderForMe() else {
                errorMsg = "No recent messages to reply to"
                return nil
            }
            return rest.isEmpty ? "@\(sender) " : "@\(sender) \(rest)"
        case "all":
            return rest.isEmpty ? "@all " : "@all \(rest)"
        case "w":
            let p = rest.components(separatedBy: " ")
            guard let worker = p.first, !worker.isEmpty else {
                errorMsg = "Usage: /w <worker> [message]"
                return nil
            }
            let body = p.dropFirst().joined(separator: " ")
            return body.isEmpty ? "@\(worker) " : "@\(worker) \(body)"
        default:
            return text
        }
    }

    private func parseMessage(_ text: String) -> (recipient: String, content: String) {
        if text.hasPrefix("@") {
            let rest = text.dropFirst()
            if let spaceIdx = rest.firstIndex(of: " ") {
                let recipient = String(rest[..<spaceIdx])
                let content = String(rest[rest.index(after: spaceIdx)...])
                    .trimmingCharacters(in: .whitespaces)
                if !recipient.isEmpty {
                    return (recipient, content.isEmpty ? text : content)
                }
            } else {
                // @recipient with no content — send the whole text as content to 'all'
                return ("all", text)
            }
        }
        return ("all", text)
    }
}
