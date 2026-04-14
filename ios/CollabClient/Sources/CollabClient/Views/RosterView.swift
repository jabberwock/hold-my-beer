import SwiftUI

struct RosterView: View {
    @ObservedObject var vm: AppViewModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack {
                Text("Team Roster")
                    .font(.headline)
                Spacer()
                // Connection indicator
                HStack(spacing: 4) {
                    Circle()
                        .fill(vm.isConnected ? Color.green : Color.orange)
                        .frame(width: 7, height: 7)
                    Text(vm.isConnected ? "live" : "reconnecting…")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)

            Divider()

            if vm.roster.isEmpty {
                Text("No workers online")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(16)
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(vm.roster) { worker in
                            WorkerRow(worker: worker)
                            Divider()
                                .padding(.leading, 44)
                        }
                    }
                }
            }
        }
    }
}

struct WorkerRow: View {
    let worker: Worker

    private var color: Color { colorForSender(worker.instanceId) }

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            // Online indicator
            Circle()
                .fill(worker.isOnline ? Color.green : Color(.systemGray4))
                .frame(width: 8, height: 8)
                .padding(.top, 5)

            VStack(alignment: .leading, spacing: 2) {
                Text(worker.instanceId)
                    .font(.subheadline.bold())
                    .foregroundStyle(color)

                if let role = worker.role, !role.isEmpty {
                    Text(role)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
            }

            Spacer()

            if let count = worker.messageCount, count > 0 {
                Text("\(count)")
                    .font(.caption2.bold())
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Color(.systemGray5))
                    .clipShape(Capsule())
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }
}
