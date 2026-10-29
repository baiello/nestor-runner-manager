import RunnerManagerKit
import SwiftUI

/// Home screen: this machine, its association with Nestor and a summary of its runners.
struct OverviewView: View {
    var model: RunnerManagerModel
    @Binding var isAddingRunner: Bool

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("This Mac").font(.largeTitle.bold())
                machineCard
                connectionCard
                runnersCard
            }
            .padding(24)
            .frame(maxWidth: 760, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .navigationTitle("This Mac")
    }

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
                InfoRow(label: "Free disk", value: Format.bytes(machine.diskFreeBytes))
            }
        }
    }

    private var connectionCard: some View {
        Card(title: "Nestor", systemImage: "link") {
            switch model.connection {
            case .loading:
                LoadingRow(text: "Checking the connection…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.load() } }
            case .loaded(.associated(let instance, let since)):
                Label("Associated", systemImage: "checkmark.seal.fill").foregroundStyle(.green)
                InfoRow(label: "Instance", value: instance)
                InfoRow(label: "Since", value: Format.relative(since))
                Text("Workspaces are assigned to runners on the Nestor web app, not on this machine.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            case .loaded(.unreachable(let instance, let reason)):
                Label("Unreachable", systemImage: "wifi.exclamationmark").foregroundStyle(.orange)
                InfoRow(label: "Instance", value: instance)
                Text(reason).font(.callout).foregroundStyle(.secondary)
            case .loaded(.notAssociated):
                Label("Not associated", systemImage: "link.badge.plus").foregroundStyle(.secondary)
                Text("Associate this machine once; Nestor then gives each runner its own identity, which can be revoked on its own. You will confirm the association in your browser — no password or token is typed here.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
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
    }

    private var runnersCard: some View {
        Card(title: "Runners", systemImage: "server.rack") {
            switch model.runners {
            case .loading:
                LoadingRow(text: "Loading the runners…")
            case .failed(let message):
                ErrorRow(message: message) { Task { await model.load() } }
            case .loaded(let runners) where runners.isEmpty:
                Text(model.isAssociated
                     ? "No runner on this machine yet. Add one to take Nestor's jobs here."
                     : "Associate this machine with Nestor to add runners.")
                    .foregroundStyle(.secondary)
                Button("Add Runner…") { isAddingRunner = true }
                    .disabled(!model.isAssociated)
            case .loaded(let runners):
                let counts = Dictionary(grouping: runners, by: \.state).mapValues(\.count)
                HStack(spacing: 20) {
                    ForEach(RunnerState.allCases, id: \.self) { state in
                        if let count = counts[state] {
                            HStack(spacing: 6) {
                                StateDot(state: state)
                                Text("\(count) \(state.title.lowercased())")
                            }
                        }
                    }
                }
                Text("Select a runner in the sidebar to see its resources, actions and logs.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
    }
}
