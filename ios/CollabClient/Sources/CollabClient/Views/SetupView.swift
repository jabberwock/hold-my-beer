import SwiftUI

struct SetupView: View {
    @ObservedObject var vm: AppViewModel
    @State private var token = ""
    @State private var serverURL = "http://localhost:8000"
    @State private var identity = "human"
    @State private var errorMsg: String?
    @State private var isTesting = false
    @State private var showToken = false
    @FocusState private var focusedField: SetupField?

    enum SetupField: Hashable {
        case token, url, identity
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 28) {
                    // Header
                    VStack(spacing: 8) {
                        Text("🍺")
                            .font(.system(size: 56))
                        Text("Hold My Beer")
                            .font(.largeTitle.bold())
                        Text("Connect to your AI team's shared switchboard.")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                            .multilineTextAlignment(.center)
                    }
                    .padding(.top, 32)

                    // Fields
                    VStack(spacing: 20) {
                        fieldGroup {
                            Label("Server Token", systemImage: "key.fill")
                                .font(.caption.bold())
                                .foregroundStyle(.secondary)
                            HStack {
                                if showToken {
                                    TextField("your-secret-token", text: $token)
                                        .font(.system(.body, design: .monospaced))
                                        .autocorrectionDisabled()
                                        .textInputAutocapitalization(.never)
                                        .focused($focusedField, equals: .token)
                                        .textContentType(.none)
                                } else {
                                    SecureField("your-secret-token", text: $token)
                                        .font(.system(.body, design: .monospaced))
                                        .focused($focusedField, equals: .token)
                                        .textContentType(.none)
                                }
                                Button(action: { showToken.toggle() }) {
                                    Image(systemName: showToken ? "eye.slash" : "eye")
                                        .foregroundStyle(.secondary)
                                }
                                Button("Generate") {
                                    token = generateToken()
                                }
                                .font(.caption.bold())
                                .foregroundStyle(.blue)
                            }
                        }
                        .onTapGesture { focusedField = .token }

                        fieldGroup {
                            Label("Server URL", systemImage: "network")
                                .font(.caption.bold())
                                .foregroundStyle(.secondary)
                            TextField("http://mbpc:8000", text: $serverURL)
                                .font(.system(.body, design: .monospaced))
                                .autocorrectionDisabled()
                                .textInputAutocapitalization(.never)
                                .keyboardType(.URL)
                                .focused($focusedField, equals: .url)
                                .textContentType(.URL)
                        }
                        .onTapGesture { focusedField = .url }

                        fieldGroup {
                            Label("Your identity in chat", systemImage: "person.fill")
                                .font(.caption.bold())
                                .foregroundStyle(.secondary)
                            TextField("human", text: $identity)
                                .autocorrectionDisabled()
                                .textInputAutocapitalization(.never)
                                .focused($focusedField, equals: .identity)
                                .textContentType(.username)
                        }
                        .onTapGesture { focusedField = .identity }
                    }

                    if let err = errorMsg {
                        Text(err)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .multilineTextAlignment(.center)
                            .padding(.horizontal)
                    }

                    // Actions
                    VStack(spacing: 12) {
                        Button(action: connect) {
                            if isTesting {
                                ProgressView()
                                    .progressViewStyle(.circular)
                                    .frame(maxWidth: .infinity)
                            } else {
                                Text("Connect")
                                    .frame(maxWidth: .infinity)
                                    .font(.headline)
                            }
                        }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.large)
                        .disabled(token.isEmpty || serverURL.isEmpty || identity.isEmpty || isTesting)
                    }
                }
                .padding(.horizontal, 24)
                .padding(.bottom, 40)
            }
            .scrollDismissesKeyboard(.interactively)
            .navigationBarTitleDisplayMode(.inline)
        }
        .onAppear {
            let cfg = vm.config
            if !cfg.token.isEmpty { token = cfg.token }
            if !cfg.serverURL.isEmpty { serverURL = cfg.serverURL }
            if !cfg.identity.isEmpty { identity = cfg.identity }
        }
    }

    private func fieldGroup<Content: View>(@ViewBuilder _ content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            content()
        }
        .padding(14)
        .background(Color(.secondarySystemBackground))
        .clipShape(RoundedRectangle(cornerRadius: 12))
    }

    private func connect() {
        let trimmedToken = token.trimmingCharacters(in: .whitespaces)
        let trimmedURL = serverURL.trimmingCharacters(in: .whitespaces)
        let trimmedID = identity.trimmingCharacters(in: .whitespaces)

        guard !trimmedToken.isEmpty else { errorMsg = "Enter or generate a token."; return }
        guard !trimmedURL.isEmpty else { errorMsg = "Enter the server URL."; return }
        guard !trimmedID.isEmpty else { errorMsg = "Enter your identity name."; return }
        guard URL(string: trimmedURL) != nil else { errorMsg = "Invalid server URL."; return }

        isTesting = true
        errorMsg = nil

        // Stage config on the API client so checkHealth can use it,
        // but don't persist (setupComplete=false) until the check passes.
        var staged = AppConfig()
        staged.token = trimmedToken
        staged.serverURL = trimmedURL
        staged.identity = trimmedID
        staged.setupComplete = false
        vm.api.config = staged

        Task {
            let ok = await vm.api.checkHealth()
            isTesting = false
            if ok {
                staged.setupComplete = true
                vm.applyConfig(staged)  // single save after confirmed reachable
            } else {
                errorMsg = "Could not reach server. Check URL and token."
            }
        }
    }

    private func generateToken() -> String {
        let bytes = (0..<32).map { _ in UInt8.random(in: 0...255) }
        return bytes.map { String(format: "%02x", $0) }.joined()
    }
}
