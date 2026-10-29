import RunnerManagerKit
import SwiftUI

extension RunnerState {
    var title: String {
        switch self {
        case .running: "Running"
        case .starting: "Starting"
        case .stopping: "Stopping"
        case .stopped: "Stopped"
        case .failed: "Failed"
        }
    }

    var color: Color {
        switch self {
        case .running: .green
        case .starting, .stopping: .orange
        case .stopped: .secondary
        case .failed: .red
        }
    }

    var symbol: String {
        switch self {
        case .running: "play.circle.fill"
        case .starting, .stopping: "clock.fill"
        case .stopped: "stop.circle.fill"
        case .failed: "exclamationmark.octagon.fill"
        }
    }
}

struct StateDot: View {
    var state: RunnerState

    var body: some View {
        Circle()
            .fill(state.color)
            .frame(width: 8, height: 8)
            .accessibilityHidden(true)
    }
}

/// The state is given by the text and the symbol as well as the color.
struct StateBadge: View {
    var state: RunnerState

    var body: some View {
        Label(state.title, systemImage: state.symbol)
            .font(.callout.weight(.medium))
            .foregroundStyle(state.color)
            .padding(.horizontal, 10)
            .padding(.vertical, 4)
            .background(state.color.opacity(0.12), in: Capsule())
    }
}

struct Card<Content: View>: View {
    var title: String
    var systemImage: String
    @ViewBuilder var content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label(title, systemImage: systemImage)
                .font(.headline)
            content
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.background.secondary, in: RoundedRectangle(cornerRadius: 10))
    }
}

struct InfoRow: View {
    var label: String
    var value: String

    var body: some View {
        LabeledContent(label) {
            Text(value).textSelection(.enabled)
        }
    }
}

struct LoadingRow: View {
    var text: String

    var body: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text(text).foregroundStyle(.secondary)
        }
    }
}

struct ErrorRow: View {
    var message: String
    var retry: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(message, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.red)
            Button("Try Again", action: retry)
        }
    }
}

enum Format {
    static func bytes(_ value: UInt64) -> String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .memory
        formatter.allowsNonnumericFormatting = false
        return formatter.string(fromByteCount: Int64(clamping: value))
    }

    static func percent(_ value: Double) -> String {
        (value / 100).formatted(.percent.precision(.fractionLength(0...1)))
    }

    static func relative(_ date: Date) -> String {
        date.formatted(.relative(presentation: .named))
    }
}
