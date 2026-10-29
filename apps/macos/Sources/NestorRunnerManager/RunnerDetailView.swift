import RunnerManagerKit
import SwiftUI

/// A runner's state, resources, actions and logs.
struct RunnerDetailView: View {
    var model: RunnerManagerModel
    var runner: Runner
    @State private var isConfirmingRemoval = false

    private var isBusy: Bool { model.busyRunners.contains(runner.id) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                header
                if runner.state == .failed, let reason = runner.failureReason {
                    Label(reason, systemImage: "exclamationmark.octagon.fill")
                        .foregroundStyle(.red)
                        .padding(12)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(.red.opacity(0.08), in: RoundedRectangle(cornerRadius: 8))
                }
                resourcesCard
                logsCard
            }
            .padding(24)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .navigationTitle(runner.name)
        .task(id: runner.id) { await model.loadLogs(runner.id) }
        .confirmationDialog(
            "Remove “\(runner.name)”?",
            isPresented: $isConfirmingRemoval,
            titleVisibility: .visible
        ) {
            Button("Remove Runner", role: .destructive) {
                Task { await model.remove(runner.id) }
            }
        } message: {
            Text(model.isDemo
                 ? "Demo: the runner is removed from this simulation only."
                 : "The runner is removed from this machine and its identity is revoked in Nestor.")
        }
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 6) {
                Text(runner.name).font(.largeTitle.bold())
                HStack(spacing: 10) {
                    StateBadge(state: runner.state)
                    Text("Created \(Format.relative(runner.createdAt))")
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            if isBusy { ProgressView().controlSize(.small) }
            if runner.state.canStop {
                Button {
                    Task { await model.stop(runner.id) }
                } label: {
                    Label("Stop", systemImage: "stop.fill")
                }
                .disabled(isBusy)
            } else {
                Button {
                    Task { await model.start(runner.id) }
                } label: {
                    Label("Start", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .disabled(isBusy || !runner.state.canStart)
            }
            Button(role: .destructive) {
                isConfirmingRemoval = true
            } label: {
                Label("Remove…", systemImage: "trash")
            }
            .disabled(isBusy || !runner.state.canRemove)
            .help(runner.state.canRemove ? "Remove this runner" : "Stop the runner before removing it")
        }
    }

    private var resourcesCard: some View {
        Card(title: "Resource usage", systemImage: "gauge.with.dots.needle.33percent") {
            InfoRow(label: "Processor", value: Format.percent(runner.usage.cpuPercent))
            InfoRow(label: "Memory", value: Format.bytes(runner.usage.memoryBytes))
            Text(model.isDemo
                 ? "Simulated measurements. A runner has no processor or memory limit."
                 : "Measured now. A runner has no processor or memory limit.")
                .font(.callout)
                .foregroundStyle(.secondary)
            Divider()
            InfoRow(label: "Job", value: runner.usage.isRunningJob ? "Running a job" : "Idle")
            InfoRow(label: "Capacity", value: "One job at a time")
            Text("The job may use several threads and processes. To run jobs in parallel, add more runners. Their isolation from each other is not guaranteed.")
                .font(.callout)
                .foregroundStyle(.secondary)
            if let lastJob = runner.lastJobAt {
                Text("Last job \(Format.relative(lastJob))")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var logsCard: some View {
        Card(title: "Logs", systemImage: "doc.text.magnifyingglass") {
            switch model.logs[runner.id] ?? .loading {
            case .loading:
                LoadingRow(text: "Loading the logs…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.loadLogs(runner.id) } }
            case .loaded(let entries) where entries.isEmpty:
                Text("No log yet.").foregroundStyle(.secondary)
            case .loaded(let entries):
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(entries.reversed()) { entry in
                        LogLine(entry: entry)
                    }
                }
                .textSelection(.enabled)
            }
        }
    }
}

struct LogLine: View {
    var entry: LogEntry

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(entry.date.formatted(date: .abbreviated, time: .standard))
                .foregroundStyle(.secondary)
            Text(entry.level.rawValue.uppercased())
                .foregroundStyle(color)
                .frame(width: 64, alignment: .leading)
            Text(entry.message)
        }
        .font(.system(.callout, design: .monospaced))
    }

    private var color: Color {
        switch entry.level {
        case .info: .secondary
        case .warning: .orange
        case .error: .red
        }
    }
}
