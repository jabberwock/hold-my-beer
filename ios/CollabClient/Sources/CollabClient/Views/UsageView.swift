import SwiftUI

struct UsageView: View {
    @ObservedObject var vm: AppViewModel
    @State private var isLoading = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack {
                Text("Usage Metrics")
                    .font(.headline)
                Spacer()
                Button(action: refresh) {
                    Image(systemName: "arrow.clockwise")
                        .foregroundStyle(.blue)
                        .rotationEffect(.degrees(isLoading ? 360 : 0))
                        .animation(isLoading ? .linear(duration: 1).repeatForever(autoreverses: false) : .default,
                                   value: isLoading)
                }
            }

            if let m = vm.metrics {
                LazyVGrid(columns: [GridItem(.flexible()), GridItem(.flexible())], spacing: 12) {
                    MetricCard(label: "Total Messages", value: "\(m.totalMessages ?? 0)", icon: "bubble.left.and.bubble.right")
                    MetricCard(label: "Active Workers", value: "\(m.activeWorkers ?? 0)", icon: "person.3")
                    MetricCard(label: "Uptime", value: m.uptimeFormatted, icon: "clock")
                    MetricCard(label: "Msgs/Hour", value: String(format: "%.1f", m.messagesPerHour ?? 0), icon: "chart.line.uptrend.xyaxis")
                }
            } else {
                Text("No metrics available")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(16)
        .onAppear { refresh() }
    }

    private func refresh() {
        isLoading = true
        Task {
            await vm.fetchMetrics()
            isLoading = false
        }
    }
}

struct MetricCard: View {
    let label: String
    let value: String
    let icon: String

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Image(systemName: icon)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Text(label)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Text(value)
                .font(.title2.bold().monospacedDigit())
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(12)
        .background(Color(.secondarySystemBackground))
        .clipShape(RoundedRectangle(cornerRadius: 10))
    }
}
