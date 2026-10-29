import RunnerManagerKit
import SwiftUI

/// Home screen: this machine. Its association with Nestor, whether it takes tasks and how many
/// at a time, the tasks running, its load and its description. Runners are managed by the
/// service and only appear in the diagnostics.
struct OverviewView: View {
    var model: RunnerManagerModel
    @Binding var selection: SidebarItem?

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("This Mac").font(.largeTitle.bold())
                connectionCard
                tasksCard
                activeTasksCard
                loadCard
                machineCard
            }
            .padding(24)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .navigationTitle("This Mac")
    }

    // MARK: Nestor

    private var connectionCard: some View {
        Card(title: "Nestor", systemImage: "link") {
            switch model.connection {
            case .loading:
                LoadingRow(text: "Checking the connection…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.load() } }
            case .loaded(.associated(let instance, let since)):
                Label(model.isDemo ? "Associated (simulated)" : "Associated", systemImage: "checkmark.seal.fill")
                    .foregroundStyle(.green)
                InfoRow(label: "Instance", value: instance)
                InfoRow(label: "Since", value: Format.relative(since))
                Text("Workspaces are assigned to this machine's runners on the Nestor web app, not here.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            case .loaded(.unreachable(let instance, let reason)):
                Label("Unreachable", systemImage: "wifi.exclamationmark").foregroundStyle(.orange)
                InfoRow(label: "Instance", value: instance)
                Text(reason).font(.callout).foregroundStyle(.secondary)
                Text("No new task is taken until Nestor is back; running tasks continue.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            case .loaded(.revoked):
                Label("Association revoked", systemImage: "xmark.seal.fill").foregroundStyle(.red)
                Text("Nestor revoked this machine. Associate it again to take tasks.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                associateButton
            case .loaded(.notAssociated):
                Label("Not associated", systemImage: "link.badge.plus").foregroundStyle(.secondary)
                Text("Associate this machine once; the service then manages its runners and Nestor gives each one its own identity. You will confirm the association in your browser — no password or token is typed here.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                associateButton
            }
        }
    }

    private var associateButton: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Button(model.isDemo ? "Associate (Simulated)" : "Associate with Nestor") {
                    Task { await model.associate() }
                }
                .buttonStyle(.borderedProminent)
                .disabled(model.isAssociating)
                if model.isAssociating { ProgressView().controlSize(.small) }
            }
            if model.isDemo {
                Text("Demo: the association is simulated and nothing is sent to Nestor.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    // MARK: Tasks

    private var tasksCard: some View {
        Card(title: "Tasks", systemImage: "square.stack.3d.up") {
            switch model.status {
            case .loading:
                LoadingRow(text: "Reading the service…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.load() } }
            case .loaded(let status):
                TasksControls(model: model, status: status)
            }
        }
    }

    private var activeTasksCard: some View {
        Card(title: "Running now", systemImage: "play.square.stack") {
            switch model.status {
            case .loading:
                LoadingRow(text: "Reading the service…")
            case .failed:
                Text("Unavailable").foregroundStyle(.secondary)
            case .loaded(let status) where status.activeTasks.isEmpty:
                Text("No task is running on this machine.").foregroundStyle(.secondary)
            case .loaded(let status):
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(status.activeTasks) { task in
                        HStack(spacing: 10) {
                            Image(systemName: "gearshape.2.fill").foregroundStyle(.green)
                                .accessibilityHidden(true)
                            Text(task.taskRef).font(.body.monospaced()).textSelection(.enabled)
                            Spacer()
                            Button(task.runnerName) { selection = .runner(task.runnerID) }
                                .buttonStyle(.link)
                                .help("Open this runner's diagnostics")
                            Text("started \(Format.relative(task.startedAt))").foregroundStyle(.secondary)
                        }
                        .accessibilityElement(children: .combine)
                    }
                }
            }
        }
    }

    // MARK: Load

    private var loadCard: some View {
        Card(title: "Load", systemImage: "gauge.with.dots.needle.33percent") {
            if let load = model.status.value?.load {
                InfoRow(label: "Processor", value: Format.percent(load.cpuPercent))
                InfoRow(
                    label: "Memory",
                    value: "\(Format.bytes(load.memoryUsedBytes)) of \(Format.bytes(load.memoryTotalBytes))"
                )
                if let pressure = load.memoryPressure {
                    InfoRow(label: "Memory pressure", value: pressure.title)
                }
                InfoRow(label: "Free disk", value: Format.bytes(load.diskFreeBytes))
                Text(model.isDemo
                     ? "Simulated measurements. Nothing here limits a runner."
                     : "Measured \(Format.relative(load.sampledAt)). Nothing here limits a runner.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else if case .loading = model.status {
                LoadingRow(text: "Measuring…")
            } else {
                Text("No measurement yet.").foregroundStyle(.secondary)
            }
        }
    }

    // MARK: Machine

    private var machineCard: some View {
        Card(title: "Machine", systemImage: "desktopcomputer") {
            switch model.machine {
            case .loading:
                LoadingRow(text: "Reading the machine…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.load() } }
            case .loaded(let machine):
                InfoRow(label: "Name", value: machine.name)
                InfoRow(label: "Model", value: machine.model)
                InfoRow(label: "System", value: "\(machine.operatingSystem) · \(machine.architecture)")
                InfoRow(label: "Processor", value: "\(machine.cpuCores) cores")
                InfoRow(label: "Memory", value: Format.bytes(machine.memoryBytes))
            }
        }
    }
}

/// Automatic / pause and the number of tasks at a time.
struct TasksControls: View {
    var model: RunnerManagerModel
    var status: MachineStatus

    var body: some View {
        if let error = status.configError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
        }
        HStack(spacing: 10) {
            Image(systemName: status.admission.symbol).foregroundStyle(status.admission.color)
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(status.admission.title).font(.headline)
                Text(status.admission.explanation)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .accessibilityElement(children: .combine)

        LabeledContent("Mode") {
            Picker("Mode", selection: Binding(
                get: { status.mode },
                set: { mode in Task { await model.setMode(mode) } }
            )) {
                Text("Automatic").tag(AdmissionMode.automatic)
                Text("Paused").tag(AdmissionMode.paused)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .frame(width: 220)
            .disabled(model.isUpdatingSettings)
        }

        LabeledContent("Tasks at a time") {
            HStack(spacing: 8) {
                Text("\(status.busyRunners) running of \(status.maxConcurrentTasks)")
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                Stepper(
                    "\(status.maxConcurrentTasks)",
                    value: Binding(
                        get: { status.maxConcurrentTasks },
                        set: { value in Task { await model.setMaxConcurrentTasks(value) } }
                    ),
                    in: 1...max(1, status.maxConcurrentTasksUpperBound)
                )
                .monospacedDigit()
                .disabled(model.isUpdatingSettings)
                .accessibilityLabel("Tasks at a time")
                .accessibilityValue("\(status.maxConcurrentTasks)")
            }
        }

        Text("This is how many tasks the machine accepts at the same time — not a processor or memory limit. Pausing or lowering it never stops a running task: it only holds back new ones. The service starts and stops runners to match.")
            .font(.callout)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
        if model.isDemo {
            Text("Demo: these settings drive a simulation; no real task is taken.")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }
}

extension AdmissionState {
    var title: String {
        switch self {
        case .open: "Taking tasks"
        case .paused: "Paused"
        case .blocked(.notAssociated): "Not taking tasks"
        case .blocked(.unreachable): "Waiting for Nestor"
        case .blocked(.revoked): "Not taking tasks"
        case .blocked(.reconciling): "Starting"
        }
    }

    var explanation: String {
        switch self {
        case .open: "New tasks are accepted up to the number below."
        case .paused: "No new task is accepted. Tasks already running finish normally."
        case .blocked(.notAssociated): "Associate this machine with Nestor first."
        case .blocked(.unreachable): "Nestor cannot be reached. Running tasks continue."
        case .blocked(.revoked): "The association was revoked in Nestor."
        case .blocked(.reconciling): "The service is checking the runners it left running."
        }
    }

    var symbol: String {
        switch self {
        case .open: "play.circle.fill"
        case .paused: "pause.circle.fill"
        case .blocked: "minus.circle.fill"
        }
    }

    var color: Color {
        switch self {
        case .open: .green
        case .paused: .orange
        case .blocked: .secondary
        }
    }
}

extension MemoryPressure {
    var title: String {
        switch self {
        case .normal: "Normal"
        case .warning: "Warning"
        case .critical: "Critical"
        }
    }
}
