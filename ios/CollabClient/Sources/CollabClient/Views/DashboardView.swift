import SwiftUI

struct DashboardView: View {
    @ObservedObject var vm: AppViewModel
    @State private var selectedTab: DashTab = .messages

    enum DashTab { case messages, roster, tasks, usage }

    var body: some View {
        TabView(selection: $selectedTab) {
            messagesTab
                .tabItem {
                    Label("Messages", systemImage: "bubble.left.and.bubble.right")
                }
                .tag(DashTab.messages)

            NavigationStack {
                RosterView(vm: vm)
            }
            .tabItem {
                Label("Team", systemImage: "person.3")
            }
            .tag(DashTab.roster)

            NavigationStack {
                TodosView(vm: vm)
            }
            .tabItem {
                Label("Tasks", systemImage: "checklist")
            }
            .tag(DashTab.tasks)

            NavigationStack {
                UsageView(vm: vm)
            }
            .tabItem {
                Label("Usage", systemImage: "chart.bar")
            }
            .tag(DashTab.usage)
        }
        .alert("Error", isPresented: $vm.showingError, actions: {
            Button("OK") {}
        }, message: {
            Text(vm.errorMessage ?? "Unknown error")
        })
    }

    private var messagesTab: some View {
        NavigationStack {
            VStack(spacing: 0) {
                // Feed filter tabs
                feedPicker
                    .padding(.horizontal, 16)
                    .padding(.vertical, 8)

                Divider()

                // Message list
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 8) {
                            ForEach(vm.filteredMessages) { msg in
                                MessageBubble(message: msg, identity: vm.config.identity)
                                    .id(msg.id)
                                    .padding(.horizontal, 12)
                            }
                        }
                        .padding(.vertical, 8)
                    }
                    .onChange(of: vm.filteredMessages.count) { _, _ in
                        if let last = vm.filteredMessages.last {
                            withAnimation { proxy.scrollTo(last.id, anchor: .bottom) }
                        }
                    }
                }

                // Compose bar
                ComposeView(vm: vm)
            }
            .navigationTitle("Hold My Beer")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .navigationBarTrailing) {
                    Button(action: { vm.stopDashboard(); vm.config.setupComplete = false; vm.config.save() }) {
                        Image(systemName: "gear")
                    }
                }
            }
        }
    }

    private var feedPicker: some View {
        HStack(spacing: 0) {
            feedTab(title: "All", tab: .all)
            feedTab(title: mentionsLabel, tab: .mentions)
        }
        .background(Color(.secondarySystemBackground))
        .clipShape(Capsule())
    }

    private var mentionsLabel: String {
        vm.unreadMentions > 0 ? "Mentions (\(vm.unreadMentions))" : "Mentions"
    }

    private func feedTab(title: String, tab: AppViewModel.FeedTab) -> some View {
        Button(action: { vm.setTab(tab) }) {
            Text(title)
                .font(.subheadline.bold())
                .padding(.horizontal, 16)
                .padding(.vertical, 7)
                .frame(maxWidth: .infinity)
                .background(vm.activeTab == tab ? Color(.systemBackground) : Color.clear)
                .clipShape(Capsule())
                .foregroundStyle(vm.activeTab == tab ? .primary : .secondary)
        }
        .animation(.easeInOut(duration: 0.15), value: vm.activeTab)
    }
}
