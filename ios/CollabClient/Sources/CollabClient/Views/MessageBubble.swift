import SwiftUI

struct MessageBubble: View {
    let message: Message
    let identity: String

    private var isMine: Bool { message.sender == identity }
    private var color: Color { colorForSender(message.sender) }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                // Sender badge
                Text(message.sender)
                    .font(.caption2.bold())
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(color.opacity(0.15))
                    .foregroundStyle(color)
                    .clipShape(Capsule())

                // Recipient
                Text(message.isToAll ? "→ all" : "→ \(message.recipient)")
                    .font(.caption2)
                    .foregroundStyle(.secondary)

                Spacer()

                Text(message.formattedTime)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }

            Text(message.content)
                .font(.body)
                .foregroundStyle(.primary)
                .textSelection(.enabled)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .background(
            RoundedRectangle(cornerRadius: 10)
                .fill(isMine
                    ? Color(.systemBlue).opacity(0.08)
                    : Color(.secondarySystemBackground))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 10)
                .stroke(isMine ? color.opacity(0.3) : Color.clear, lineWidth: 1)
        )
    }
}
