import SwiftUI

struct TodosView: View {
    @ObservedObject var vm: AppViewModel
    @State private var showAddForm = false
    @State private var newTodoInstance = ""
    @State private var newTodoDesc = ""
    @State private var isAdding = false
    @State private var errorMsg: String?
    @State private var mentionText = ""
    @State private var mentionMatches: [String] = []
    @State private var showMentions = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Header
            HStack {
                Text("Tasks")
                    .font(.headline)
                Spacer()
                Text("\(vm.todos.count)")
                    .font(.caption.bold())
                    .padding(.horizontal, 7)
                    .padding(.vertical, 3)
                    .background(Color(.systemGray5))
                    .clipShape(Capsule())
                Button(action: { withAnimation { showAddForm.toggle() } }) {
                    Image(systemName: showAddForm ? "minus.circle" : "plus.circle")
                        .foregroundStyle(.blue)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)

            if showAddForm {
                addForm
                    .transition(.move(edge: .top).combined(with: .opacity))
                Divider()
            }

            Divider()

            if vm.todos.isEmpty {
                Text("No pending tasks")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(16)
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(vm.todos) { todo in
                            TodoRow(todo: todo, onComplete: {
                                Task { try? await vm.completeTodo(hash: todo.hash) }
                            })
                            Divider()
                                .padding(.leading, 16)
                        }
                    }
                }
            }
        }
        .animation(.easeInOut(duration: 0.2), value: showAddForm)
    }

    private var addForm: some View {
        VStack(alignment: .leading, spacing: 10) {
            // Mention autocomplete
            if showMentions && !mentionMatches.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 8) {
                        ForEach(mentionMatches, id: \.self) { name in
                            Button("@\(name)") {
                                applyMention(name)
                            }
                            .font(.caption.bold())
                            .padding(.horizontal, 10)
                            .padding(.vertical, 5)
                            .background(Color.blue.opacity(0.12))
                            .foregroundStyle(.blue)
                            .clipShape(Capsule())
                        }
                    }
                    .padding(.horizontal, 14)
                }
            }

            // Worker picker
            Picker("Assign to", selection: $newTodoInstance) {
                Text("Assign to…").tag("")
                ForEach(vm.roster) { worker in
                    Text("@\(worker.instanceId)").tag(worker.instanceId)
                }
            }
            .pickerStyle(.menu)
            .padding(.horizontal, 14)

            // Description
            VStack(alignment: .leading, spacing: 4) {
                TextField("Task description…", text: $newTodoDesc, axis: .vertical)
                    .lineLimit(2...5)
                    .padding(10)
                    .background(Color(.secondarySystemBackground))
                    .clipShape(RoundedRectangle(cornerRadius: 8))
                    .onChange(of: newTodoDesc) { _, val in onDescChange(val) }
            }
            .padding(.horizontal, 14)

            if let err = errorMsg {
                Text(err).font(.caption).foregroundStyle(.red).padding(.horizontal, 14)
            }

            Button(action: submitTodo) {
                if isAdding {
                    ProgressView().frame(maxWidth: .infinity)
                } else {
                    Text("Add Task").frame(maxWidth: .infinity)
                }
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.regular)
            .disabled(newTodoInstance.isEmpty || newTodoDesc.isEmpty || isAdding)
            .padding(.horizontal, 14)
            .padding(.bottom, 10)
        }
        .padding(.top, 10)
        .background(Color(.systemBackground))
    }

    private func onDescChange(_ val: String) {
        // @ mention in todo description
        let names = vm.workerNames.filter { $0 != "all" }
        if let atRange = findAtRange(in: val) {
            let query = String(val[atRange...]).lowercased()
            let matches = names.filter { $0.lowercased().hasPrefix(query) }
            if !matches.isEmpty {
                mentionMatches = matches
                showMentions = true
                return
            }
        }
        showMentions = false
    }

    private func findAtRange(in text: String) -> String.Index? {
        var idx = text.endIndex
        while idx > text.startIndex {
            let prev = text.index(before: idx)
            if text[prev] == "@" { return idx }
            if text[prev].isWhitespace { return nil }
            idx = prev
        }
        return nil
    }

    private func applyMention(_ name: String) {
        guard let start = findAtRange(in: newTodoDesc) else { return }
        let atIdx = newTodoDesc.index(before: start)
        let before = String(newTodoDesc[..<atIdx])
        newTodoDesc = before + "@\(name) "
        showMentions = false
    }

    private func submitTodo() {
        guard !newTodoInstance.isEmpty, !newTodoDesc.isEmpty else { return }
        guard newTodoDesc.count <= 500 else {
            errorMsg = "Description must be 500 characters or fewer."
            return
        }
        isAdding = true
        errorMsg = nil
        Task {
            do {
                try await vm.addTodo(instance: newTodoInstance, description: newTodoDesc)
                newTodoDesc = ""
                newTodoInstance = ""
                showAddForm = false
            } catch {
                errorMsg = error.localizedDescription
            }
            isAdding = false
        }
    }
}

struct TodoRow: View {
    let todo: TodoItem
    let onComplete: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Button(action: onComplete) {
                Image(systemName: "circle")
                    .font(.system(size: 18))
                    .foregroundStyle(.blue)
            }
            .padding(.top, 2)

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 4) {
                    Text("@\(todo.instance)")
                        .font(.caption.bold())
                        .foregroundStyle(colorForSender(todo.instance))
                    Text("·")
                        .foregroundStyle(.secondary)
                    Text("from \(todo.assignedBy)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Text("·")
                        .foregroundStyle(.secondary)
                    Text(todo.timeAgo)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text(todo.description)
                    .font(.subheadline)
                    .foregroundStyle(.primary)
            }

            Spacer()
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }
}
